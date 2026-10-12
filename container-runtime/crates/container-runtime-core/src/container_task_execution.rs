//! Execution-local CTI graph state. RELC owns the graph; Controller only
//! validates its on-disk shape and advances compiled edges. No REL parsing,
//! SourceId discovery, or dynamic OID allocation occurs here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ipc_protocol::{
    CtiNodeKind, CtiSectionKind, Slot, SlotTable, SlotType, TaskArena, CTI_MAX_ARENA_BYTES,
    CTI_MAX_SLOTS,
};

use crate::LoadedTaskImage;

const CTI_PAYLOAD_VERSION: u16 = 1;
const NO_EDGE: u32 = u32::MAX;
const MAX_GRAPH_NODES: usize = 16_384;

#[derive(Debug, Clone)]
pub struct TaskGraphNode {
    pub id: u32,
    pub kind: CtiNodeKind,
    pub dependencies: BTreeSet<u32>,
    pub success: Option<u32>,
    pub failure: Option<u32>,
    pub timeout: Option<u32>,
    pub cancelled: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CompiledTaskGraph {
    pub entry: u32,
    pub nodes: BTreeMap<u32, TaskGraphNode>,
}

impl CompiledTaskGraph {
    pub fn from_task(task: &LoadedTaskImage) -> Result<Self, String> {
        let image = task.image();
        let bytes = image
            .sections
            .iter()
            .find(|section| section.kind == CtiSectionKind::Graph.code())
            .ok_or("CTI has no required GRAPH section")?;
        let mut reader = Reader::new(&bytes.data);
        if reader.u16()? != CTI_PAYLOAD_VERSION || reader.u16()? != 0 {
            return Err("unsupported CTI GRAPH ABI".into());
        }
        let entry = reader.u32()?;
        if entry != image.header.entry_node {
            return Err("CTI GRAPH entry does not match CTI header".into());
        }
        let count = reader.u32()? as usize;
        if count == 0 || count > MAX_GRAPH_NODES {
            return Err("CTI GRAPH node count is outside permitted range".into());
        }
        let mut nodes = BTreeMap::new();
        for _ in 0..count {
            let id = reader.u32()?;
            let kind = CtiNodeKind::from_code(reader.u16()?)
                .ok_or_else(|| format!("unsupported compiled CTI node kind at {id}"))?;
            let dependency_count = reader.u16()? as usize;
            if dependency_count > count {
                return Err(format!("node {id} has excessive dependencies"));
            }
            let success = edge(reader.u32()?);
            let failure = edge(reader.u32()?);
            let timeout = edge(reader.u32()?);
            let cancelled = edge(reader.u32()?);
            let mut dependencies = BTreeSet::new();
            for _ in 0..dependency_count {
                let parent = reader.u32()?;
                if parent == id || !dependencies.insert(parent) {
                    return Err(format!("node {id} has invalid/duplicate dependencies"));
                }
            }
            if nodes
                .insert(
                    id,
                    TaskGraphNode {
                        id,
                        kind,
                        dependencies,
                        success,
                        failure,
                        timeout,
                        cancelled,
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate node ID {id}"));
            }
        }
        reader.finish()?;
        if !nodes.contains_key(&entry) {
            return Err("CTI GRAPH does not contain its entry".into());
        }
        for node in nodes.values() {
            for id in node.dependencies.iter().copied().chain(
                [node.success, node.failure, node.timeout, node.cancelled]
                    .into_iter()
                    .flatten(),
            ) {
                if !nodes.contains_key(&id) {
                    return Err(format!("node {} references missing node {id}", node.id));
                }
            }
        }
        // An activation edge cannot reach itself transitively. Reject cycles
        // before ExecuteTask can consume any worker resources.
        fn visit(
            id: u32,
            nodes: &BTreeMap<u32, TaskGraphNode>,
            visiting: &mut BTreeSet<u32>,
            visited: &mut BTreeSet<u32>,
        ) -> Result<(), String> {
            if visited.contains(&id) {
                return Ok(());
            }
            if !visiting.insert(id) {
                return Err(format!("CTI GRAPH contains a cycle at node {id}"));
            }
            let node = &nodes[&id];
            for next in [node.success, node.failure, node.timeout, node.cancelled]
                .into_iter()
                .flatten()
            {
                visit(next, nodes, visiting, visited)?;
            }
            visiting.remove(&id);
            visited.insert(id);
            Ok(())
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for id in nodes.keys().copied() {
            visit(id, &nodes, &mut visiting, &mut visited)?;
        }
        Ok(Self { entry, nodes })
    }

    /// Compiler-assigned root provenance from the immutable optional debug
    /// section. This is identity, not capability authority.
    pub fn root_source_id(task: &LoadedTaskImage) -> Result<String, String> {
        let image = task.image();
        let section = image
            .sections
            .iter()
            .find(|section| section.kind == CtiSectionKind::DebugNames.code())
            .ok_or("CTI does not contain root source identity")?;
        let mut reader = Reader::new(&section.data);
        if reader.u16()? != CTI_PAYLOAD_VERSION || reader.u16()? != 0 {
            return Err("CTI DebugNames ABI mismatch".into());
        }
        let _canonical = reader.string()?;
        let _display = reader.string()?;
        let source = reader.string()?;
        if source.is_empty() || source.len() > 512 || source.chars().any(char::is_control) {
            return Err("CTI root SourceId is invalid".into());
        }
        Ok(source)
    }

    /// Do not infer authority from compiled static capabilities. Until the
    /// trusted capability-node adapters are present, only capability-free
    /// single WASM Tasks may take the direct isolated-worker path.
    pub fn has_no_capability_nodes(task: &LoadedTaskImage) -> Result<bool, String> {
        let image = task.image();
        let section = image
            .sections
            .iter()
            .find(|section| section.kind == CtiSectionKind::Capabilities.code())
            .ok_or("CTI does not contain CAPABILITIES")?;
        let mut reader = Reader::new(&section.data);
        if reader.u16()? != CTI_PAYLOAD_VERSION {
            return Err("CTI CAPABILITIES ABI mismatch".into());
        }
        Ok(reader.u16()? == 0)
    }

    /// Return the embedded WASM payload only if RELC emitted a real artifact;
    /// a Service-only/no-code Task must never be treated as an empty WASM.
    pub fn embedded_wasm(task: &LoadedTaskImage) -> Result<Option<Vec<u8>>, String> {
        let image = task.image();
        let section = image
            .sections
            .iter()
            .find(|section| section.kind == CtiSectionKind::Code.code())
            .ok_or("CTI has no CODE section")?;
        let mut reader = Reader::new(&section.data);
        if reader.u16()? != CTI_PAYLOAD_VERSION {
            return Err("unsupported CTI CODE ABI".into());
        }
        let _task_kind = reader.u16()?;
        let has_wasm = reader.u8()?;
        let padding = [reader.u8()?, reader.u8()?, reader.u8()?];
        if padding != [0, 0, 0] {
            return Err("CTI CODE reserved fields are not zero".into());
        }
        let expected = reader.take(32)?;
        let len = reader.u32()? as usize;
        if len > ipc_protocol::MAX_ARTIFACT_BYTES {
            return Err("CTI embedded WASM exceeds Controller artifact limit".into());
        }
        let code = reader.take(len)?.to_vec();
        if has_wasm == 0 {
            if len != 0 || expected.iter().any(|byte| *byte != 0) {
                return Err("CTI CODE absent artifact has nonempty payload".into());
            }
            return Ok(None);
        }
        if has_wasm != 1 || ipc_protocol::cti_sha256(&code).as_slice() != expected {
            return Err("CTI CODE artifact hash mismatch".into());
        }
        Ok(Some(code))
    }
}

pub struct TaskExecution {
    pub task: Arc<LoadedTaskImage>,
    pub graph: CompiledTaskGraph,
    pub slots: SlotTable,
    pub arena: TaskArena,
    ready: VecDeque<u32>,
    armed: BTreeSet<u32>,
    completed: BTreeSet<u32>,
    output: Vec<u8>,
    deadline: Instant,
}

impl TaskExecution {
    pub fn new(
        task: Arc<LoadedTaskImage>,
        input: &[u8],
        timeout: Duration,
    ) -> Result<Self, String> {
        let graph = CompiledTaskGraph::from_task(&task)?;
        let image = task.image();
        let layout = image
            .sections
            .iter()
            .find(|section| section.kind == CtiSectionKind::SlotLayout.code())
            .ok_or("CTI has no SLOT_LAYOUT section")?;
        let mut layout = Reader::new(&layout.data);
        if layout.u16()? != CTI_PAYLOAD_VERSION || layout.u16()? != 0 {
            return Err("unsupported CTI SLOT_LAYOUT ABI".into());
        }
        let arena_limit = layout.u32()? as usize;
        let slot_count = layout.u16()? as usize;
        if layout.u16()? != 0
            || arena_limit == 0
            || arena_limit > CTI_MAX_ARENA_BYTES
            || slot_count < 2
            || slot_count > CTI_MAX_SLOTS
        {
            return Err("CTI SLOT_LAYOUT has invalid limits".into());
        }
        layout.finish()?;
        let mut slots = SlotTable::new(slot_count).map_err(|e| e.to_string())?;
        let mut arena = TaskArena::with_limit(arena_limit).map_err(|e| e.to_string())?;
        let input_ref = arena.push(input).map_err(|e| e.to_string())?;
        slots
            .set(
                0,
                Slot::arena(SlotType::Bytes, input_ref).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        let mut ready = VecDeque::new();
        ready.push_back(graph.entry);
        let mut armed = BTreeSet::new();
        armed.insert(graph.entry);
        Ok(Self {
            task,
            graph,
            slots,
            arena,
            ready,
            armed,
            completed: BTreeSet::new(),
            output: input.to_vec(),
            deadline: Instant::now() + timeout,
        })
    }

    /// Execute the compiler DAG. A caller-provided trusted node adapter
    /// performs the actual WASM/Service/QuickDB/capability work. The graph
    /// runner never resolves host services or grants capabilities itself.
    pub fn run(
        &mut self,
        cancelled: &AtomicBool,
        mut dispatch: impl FnMut(CtiNodeKind, &LoadedTaskImage, &[u8]) -> Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, String> {
        while let Some(id) = self.ready.pop_front() {
            if self.completed.contains(&id) {
                return Err(format!("compiled CTI node {id} was scheduled twice"));
            }
            let node = self
                .graph
                .nodes
                .get(&id)
                .ok_or_else(|| format!("CTI node {id} disappeared"))?
                .clone();
            if !node
                .dependencies
                .iter()
                .all(|dep| self.completed.contains(dep))
            {
                // Dependency may become ready after another armed node.
                self.ready.push_back(id);
                if self.ready.iter().all(|candidate| {
                    self.graph.nodes[candidate]
                        .dependencies
                        .iter()
                        .any(|dep| !self.completed.contains(dep))
                }) {
                    return Err(format!("CTI graph node {id} has unsatisfied dependencies"));
                }
                continue;
            }
            if cancelled.load(Ordering::Acquire) {
                if let Some(target) = node.cancelled {
                    self.arm(target);
                }
                return Err("Task execution cancelled".into());
            }
            if Instant::now() >= self.deadline {
                if let Some(target) = node.timeout {
                    self.arm(target);
                }
                return Err("Task execution deadline exceeded".into());
            }
            match node.kind {
                CtiNodeKind::Return => return Ok(self.output.clone()),
                CtiNodeKind::Fail => return Err("compiled CTI FAIL node reached".into()),
                kind => match dispatch(kind, &self.task, &self.output) {
                    Ok(output) => {
                        if output.len() > ipc_protocol::MAX_EXECUTION_OUTPUT_BYTES {
                            return Err("Task output exceeds Controller IPC limit".into());
                        }
                        self.output = output;
                        let location = self.arena.push(&self.output).map_err(|e| e.to_string())?;
                        self.slots
                            .set(
                                1,
                                Slot::arena(SlotType::Bytes, location)
                                    .map_err(|e| e.to_string())?,
                            )
                            .map_err(|e| e.to_string())?;
                        self.completed.insert(id);
                        if let Some(next) = node.success {
                            self.arm(next);
                        }
                    }
                    Err(error) => {
                        self.completed.insert(id);
                        if let Some(next) = node.failure {
                            self.arm(next);
                        } else {
                            return Err(error);
                        }
                    }
                },
            }
        }
        Err("compiled CTI execution terminated without RETURN".into())
    }

    fn arm(&mut self, id: u32) {
        if self.armed.insert(id) {
            self.ready.push_back(id);
        }
    }

    /// Conservative standalone WASM path: exactly one WASM_BLOCK and a
    /// compiler-declared RETURN/FAIL, with no host capability graph nodes.
    /// Refuse other shapes rather than silently skipping compiler semantics.
    pub fn is_single_wasm_graph(&self) -> bool {
        let root = &self.graph.nodes[&self.graph.entry];
        if root.kind != CtiNodeKind::WasmBlock {
            return false;
        }
        let Some(success) = root.success else {
            return false;
        };
        if self.graph.nodes.get(&success).map(|n| n.kind) != Some(CtiNodeKind::Return) {
            return false;
        }
        let allowed = match root.failure {
            Some(failure)
                if self.graph.nodes.get(&failure).map(|n| n.kind) == Some(CtiNodeKind::Fail) =>
            {
                3
            }
            None => 2,
            _ => return false,
        };
        self.graph.nodes.len() == allowed
    }
}

fn edge(value: u32) -> Option<u32> {
    if value == NO_EDGE {
        None
    } else {
        Some(value)
    }
}

struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or("CTI section length overflow")?;
        let bytes = self
            .data
            .get(self.offset..end)
            .ok_or("truncated CTI section")?;
        self.offset = end;
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| "CTI u16 decode")?,
        ))
    }
    fn string(&mut self) -> Result<String, String> {
        let len = self.u32()? as usize;
        if len > 8192 {
            return Err("CTI string is too long".into());
        }
        let text = self.take(len)?;
        std::str::from_utf8(text)
            .map(str::to_owned)
            .map_err(|_| "CTI string is not UTF-8".into())
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| "CTI u32 decode")?,
        ))
    }
    fn finish(&self) -> Result<(), String> {
        if self.offset == self.data.len() {
            Ok(())
        } else {
            Err("CTI section has trailing bytes".into())
        }
    }
}
