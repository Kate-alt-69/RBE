use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

const LN_2: f64 = std::f64::consts::LN_2;
const DEFAULT_GROWTH_FACTOR: usize = 2;
const DEFAULT_TIGHTENING_RATIO: f64 = 0.5;
const MAX_HASH_FUNCTIONS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    Bloom,
    CountingBloom,
    ScalableBloom,
}

impl FilterKind {
    pub fn parse(value: &str) -> Result<Self, QuickDbError> {
        match value {
            "bloom" => Ok(Self::Bloom),
            "counting" | "counting-bloom" | "countingBloom" => Ok(Self::CountingBloom),
            "scalable" | "scalable-bloom" | "scalableBloom" => Ok(Self::ScalableBloom),
            other => Err(QuickDbError::new(format!(
                "unsupported quickDB filter kind {other:?}; expected bloom, counting-bloom, or scalable-bloom"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bloom => "bloom",
            Self::CountingBloom => "counting-bloom",
            Self::ScalableBloom => "scalable-bloom",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilterConfig {
    pub kind: FilterKind,
    pub capacity: usize,
    pub false_positive_rate: f64,
}

impl FilterConfig {
    pub fn validate(&self) -> Result<(), QuickDbError> {
        if self.capacity == 0 {
            return Err(QuickDbError::new("quickDB capacity must be greater than zero"));
        }
        if !self.false_positive_rate.is_finite()
            || self.false_positive_rate <= 0.0
            || self.false_positive_rate >= 1.0
        {
            return Err(QuickDbError::new(
                "quickDB falsePositiveRate must be a finite number greater than 0 and less than 1",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilterStats {
    pub kind: FilterKind,
    pub capacity: usize,
    pub writes: u64,
    pub bit_slots: usize,
    pub allocated_bytes: usize,
    pub hash_functions: u32,
    pub layers: usize,
    pub target_false_positive_rate: f64,
}

#[derive(Debug, Clone)]
pub struct QuickDbError {
    message: String,
}

impl QuickDbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for QuickDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for QuickDbError {}

#[derive(Clone, Default)]
pub struct QuickDb {
    filters: Arc<RwLock<HashMap<String, ManagedFilter>>>,
}

impl QuickDb {
    pub fn create(&self, name: &str, config: FilterConfig) -> Result<(), QuickDbError> {
        validate_name(name)?;
        config.validate()?;

        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        if filters.contains_key(name) {
            return Err(QuickDbError::new(format!(
                "quickDB filter {name:?} already exists"
            )));
        }

        let filter = match config.kind {
            FilterKind::Bloom => Filter::Bloom(BloomFilter::new(
                config.capacity,
                config.false_positive_rate,
            )?),
            FilterKind::CountingBloom => Filter::CountingBloom(CountingBloomFilter::new(
                config.capacity,
                config.false_positive_rate,
            )?),
            FilterKind::ScalableBloom => Filter::ScalableBloom(ScalableBloomFilter::new(
                config.capacity,
                config.false_positive_rate,
            )?),
        };

        filters.insert(
            name.to_string(),
            ManagedFilter {
                filter,
                ready: false,
                poisoned: false,
            },
        );
        Ok(())
    }

    pub fn add(&self, name: &str, value: &str) -> Result<(), QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters
            .get_mut(name)
            .ok_or_else(|| missing_filter(name))?;
        if let Err(error) = managed.filter.add(value.as_bytes()) {
            managed.ready = false;
            managed.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    pub fn add_many<'a>(
        &self,
        name: &str,
        values: impl IntoIterator<Item = &'a str>,
    ) -> Result<usize, QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters
            .get_mut(name)
            .ok_or_else(|| missing_filter(name))?;
        let mut added = 0usize;
        for value in values {
            if let Err(error) = managed.filter.add(value.as_bytes()) {
                managed.ready = false;
                managed.poisoned = true;
                return Err(error);
            }
            added = added.saturating_add(1);
        }
        Ok(added)
    }

    pub fn load_snapshot<'a>(
        &self,
        name: &str,
        values: impl IntoIterator<Item = &'a str>,
    ) -> Result<usize, QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters
            .get_mut(name)
            .ok_or_else(|| missing_filter(name))?;
        if managed.ready {
            return Err(QuickDbError::new(
                "quickDB.load() requires an unready filter; use rebuild() to replace a ready filter",
            ));
        }
        if managed.poisoned {
            return Err(QuickDbError::new(format!(
                "quickDB filter {name:?} cannot load into a failed snapshot; clear or rebuild it from the authoritative database first"
            )));
        }

        let mut added = 0usize;
        for value in values {
            if let Err(error) = managed.filter.add(value.as_bytes()) {
                managed.ready = false;
                managed.poisoned = true;
                return Err(error);
            }
            added = added.saturating_add(1);
        }
        managed.ready = true;
        Ok(added)
    }

    pub fn rebuild_snapshot<'a>(
        &self,
        name: &str,
        values: impl IntoIterator<Item = &'a str>,
    ) -> Result<usize, QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters
            .get_mut(name)
            .ok_or_else(|| missing_filter(name))?;
        managed.filter.clear();
        managed.ready = false;
        managed.poisoned = false;

        let mut added = 0usize;
        for value in values {
            if let Err(error) = managed.filter.add(value.as_bytes()) {
                managed.ready = false;
                managed.poisoned = true;
                return Err(error);
            }
            added = added.saturating_add(1);
        }
        managed.ready = true;
        Ok(added)
    }

    pub fn might_contain(&self, name: &str, value: &str) -> Result<bool, QuickDbError> {
        let filters = self
            .filters
            .read()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters.get(name).ok_or_else(|| missing_filter(name))?;
        ensure_ready(name, managed)?;
        Ok(managed.filter.might_contain(value.as_bytes()))
    }

    pub fn definitely_missing(&self, name: &str, value: &str) -> Result<bool, QuickDbError> {
        self.might_contain(name, value).map(|present| !present)
    }

    pub fn remove(&self, name: &str, value: &str) -> Result<bool, QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters
            .get_mut(name)
            .ok_or_else(|| missing_filter(name))?;
        ensure_ready(name, managed)?;
        managed.filter.remove(value.as_bytes())
    }

    pub fn clear(&self, name: &str) -> Result<(), QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters
            .get_mut(name)
            .ok_or_else(|| missing_filter(name))?;
        managed.filter.clear();
        managed.ready = false;
        managed.poisoned = false;
        Ok(())
    }

    pub fn drop_filter(&self, name: &str) -> Result<bool, QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        Ok(filters.remove(name).is_some())
    }

    pub fn stats(&self, name: &str) -> Result<FilterStats, QuickDbError> {
        let filters = self
            .filters
            .read()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters.get(name).ok_or_else(|| missing_filter(name))?;
        Ok(managed.filter.stats())
    }

    pub fn seal(&self, name: &str) -> Result<(), QuickDbError> {
        let mut filters = self
            .filters
            .write()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters
            .get_mut(name)
            .ok_or_else(|| missing_filter(name))?;
        if managed.poisoned {
            return Err(QuickDbError::new(format!(
                "quickDB filter {name:?} cannot be sealed after a failed mutation; clear or rebuild it from the authoritative database first"
            )));
        }
        managed.ready = true;
        Ok(())
    }

    pub fn is_ready(&self, name: &str) -> Result<bool, QuickDbError> {
        let filters = self
            .filters
            .read()
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))?;
        let managed = filters.get(name).ok_or_else(|| missing_filter(name))?;
        Ok(managed.ready)
    }

    pub fn len(&self) -> Result<usize, QuickDbError> {
        self.filters
            .read()
            .map(|filters| filters.len())
            .map_err(|_| QuickDbError::new("quickDB registry lock is poisoned"))
    }
}

fn validate_name(name: &str) -> Result<(), QuickDbError> {
    if name.trim().is_empty() {
        Err(QuickDbError::new("quickDB filter name cannot be empty"))
    } else if name.len() > 128 {
        Err(QuickDbError::new(
            "quickDB filter name cannot exceed 128 bytes",
        ))
    } else {
        Ok(())
    }
}

fn ensure_ready(name: &str, managed: &ManagedFilter) -> Result<(), QuickDbError> {
    if managed.ready {
        Ok(())
    } else {
        Err(QuickDbError::new(format!(
            "quickDB filter {name:?} is not ready; finish rebuilding it and call quickDB.seal() before membership checks"
        )))
    }
}

fn missing_filter(name: &str) -> QuickDbError {
    QuickDbError::new(format!(
        "quickDB filter {name:?} does not exist; call quickDB.create() first"
    ))
}

struct ManagedFilter {
    filter: Filter,
    ready: bool,
    poisoned: bool,
}

enum Filter {
    Bloom(BloomFilter),
    CountingBloom(CountingBloomFilter),
    ScalableBloom(ScalableBloomFilter),
}

impl Filter {
    fn add(&mut self, value: &[u8]) -> Result<(), QuickDbError> {
        match self {
            Self::Bloom(filter) => {
                filter.add(value);
                Ok(())
            }
            Self::CountingBloom(filter) => filter.add(value),
            Self::ScalableBloom(filter) => filter.add(value),
        }
    }

    fn might_contain(&self, value: &[u8]) -> bool {
        match self {
            Self::Bloom(filter) => filter.might_contain(value),
            Self::CountingBloom(filter) => filter.might_contain(value),
            Self::ScalableBloom(filter) => filter.might_contain(value),
        }
    }

    fn remove(&mut self, value: &[u8]) -> Result<bool, QuickDbError> {
        match self {
            Self::CountingBloom(filter) => Ok(filter.remove(value)),
            Self::Bloom(_) | Self::ScalableBloom(_) => Err(QuickDbError::new(
                "quickDB.remove() requires a counting-bloom filter",
            )),
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Bloom(filter) => filter.clear(),
            Self::CountingBloom(filter) => filter.clear(),
            Self::ScalableBloom(filter) => filter.clear(),
        }
    }

    fn stats(&self) -> FilterStats {
        match self {
            Self::Bloom(filter) => filter.stats(FilterKind::Bloom),
            Self::CountingBloom(filter) => filter.stats(),
            Self::ScalableBloom(filter) => filter.stats(),
        }
    }
}

struct BloomFilter {
    words: Vec<u64>,
    bit_len: usize,
    hash_functions: u32,
    capacity: usize,
    target_false_positive_rate: f64,
    writes: u64,
}

impl BloomFilter {
    fn new(capacity: usize, false_positive_rate: f64) -> Result<Self, QuickDbError> {
        let (bit_len, hash_functions) = bloom_shape(capacity, false_positive_rate)?;
        let word_len = bit_len.div_ceil(64);
        let mut words = Vec::new();
        words
            .try_reserve_exact(word_len)
            .map_err(|_| QuickDbError::new("quickDB could not allocate bloom filter memory"))?;
        words.resize(word_len, 0u64);
        Ok(Self {
            words,
            bit_len,
            hash_functions,
            capacity,
            target_false_positive_rate: false_positive_rate,
            writes: 0,
        })
    }

    fn add(&mut self, value: &[u8]) {
        let (first, second) = hash_pair(value);
        for index in hash_indexes(first, second, self.hash_functions, self.bit_len) {
            let word = index / 64;
            let bit = index % 64;
            self.words[word] |= 1u64 << bit;
        }
        self.writes = self.writes.saturating_add(1);
    }

    fn might_contain(&self, value: &[u8]) -> bool {
        let (first, second) = hash_pair(value);
        hash_indexes(first, second, self.hash_functions, self.bit_len).all(|index| {
            let word = index / 64;
            let bit = index % 64;
            self.words[word] & (1u64 << bit) != 0
        })
    }

    fn clear(&mut self) {
        self.words.fill(0);
        self.writes = 0;
    }

    fn stats(&self, kind: FilterKind) -> FilterStats {
        FilterStats {
            kind,
            capacity: self.capacity,
            writes: self.writes,
            bit_slots: self.bit_len,
            allocated_bytes: self.words.len().saturating_mul(std::mem::size_of::<u64>()),
            hash_functions: self.hash_functions,
            layers: 1,
            target_false_positive_rate: self.target_false_positive_rate,
        }
    }
}

struct CountingBloomFilter {
    counters: Vec<u8>,
    slot_len: usize,
    hash_functions: u32,
    capacity: usize,
    target_false_positive_rate: f64,
    writes: u64,
}

impl CountingBloomFilter {
    fn new(capacity: usize, false_positive_rate: f64) -> Result<Self, QuickDbError> {
        let (slot_len, hash_functions) = bloom_shape(capacity, false_positive_rate)?;
        let counter_len = slot_len.div_ceil(2);
        let mut counters = Vec::new();
        counters
            .try_reserve_exact(counter_len)
            .map_err(|_| QuickDbError::new("quickDB could not allocate counting-bloom memory"))?;
        counters.resize(counter_len, 0u8);
        Ok(Self {
            counters,
            slot_len,
            hash_functions,
            capacity,
            target_false_positive_rate: false_positive_rate,
            writes: 0,
        })
    }

    fn add(&mut self, value: &[u8]) -> Result<(), QuickDbError> {
        let (first, second) = hash_pair(value);
        let indexes: Vec<_> =
            hash_indexes(first, second, self.hash_functions, self.slot_len).collect();
        for &index in &indexes {
            let increments = indexes.iter().filter(|&&other| other == index).count() as u8;
            let current = self.counter(index);
            if current > 15u8.saturating_sub(increments) {
                return Err(QuickDbError::new(
                    "quickDB counting-bloom counter saturated; rebuild the filter before using negative membership results",
                ));
            }
        }
        for index in indexes {
            let current = self.counter(index);
            self.set_counter(index, current + 1);
        }
        self.writes = self.writes.saturating_add(1);
        Ok(())
    }

    fn might_contain(&self, value: &[u8]) -> bool {
        let (first, second) = hash_pair(value);
        hash_indexes(first, second, self.hash_functions, self.slot_len)
            .all(|index| self.counter(index) != 0)
    }

    fn remove(&mut self, value: &[u8]) -> bool {
        let (first, second) = hash_pair(value);
        let indexes: Vec<_> =
            hash_indexes(first, second, self.hash_functions, self.slot_len).collect();

        for &index in &indexes {
            let decrements = indexes.iter().filter(|&&other| other == index).count() as u8;
            if self.counter(index) < decrements {
                return false;
            }
        }

        for index in indexes {
            let current = self.counter(index);
            self.set_counter(index, current - 1);
        }
        true
    }

    fn clear(&mut self) {
        self.counters.fill(0);
        self.writes = 0;
    }

    fn counter(&self, index: usize) -> u8 {
        let byte = self.counters[index / 2];
        if index % 2 == 0 {
            byte & 0x0f
        } else {
            (byte >> 4) & 0x0f
        }
    }

    fn set_counter(&mut self, index: usize, value: u8) {
        let byte = &mut self.counters[index / 2];
        let value = value & 0x0f;
        if index % 2 == 0 {
            *byte = (*byte & 0xf0) | value;
        } else {
            *byte = (*byte & 0x0f) | (value << 4);
        }
    }

    fn stats(&self) -> FilterStats {
        FilterStats {
            kind: FilterKind::CountingBloom,
            capacity: self.capacity,
            writes: self.writes,
            bit_slots: self.slot_len,
            allocated_bytes: self.counters.len(),
            hash_functions: self.hash_functions,
            layers: 1,
            target_false_positive_rate: self.target_false_positive_rate,
        }
    }
}

struct ScalableBloomFilter {
    layers: Vec<BloomFilter>,
    initial_capacity: usize,
    target_false_positive_rate: f64,
}

impl ScalableBloomFilter {
    fn new(capacity: usize, false_positive_rate: f64) -> Result<Self, QuickDbError> {
        let first_rate = false_positive_rate * (1.0 - DEFAULT_TIGHTENING_RATIO);
        Ok(Self {
            layers: vec![BloomFilter::new(capacity, first_rate)?],
            initial_capacity: capacity,
            target_false_positive_rate: false_positive_rate,
        })
    }

    fn add(&mut self, value: &[u8]) -> Result<(), QuickDbError> {
        let needs_layer = self
            .layers
            .last()
            .map(|layer| layer.writes >= layer.capacity as u64)
            .unwrap_or(true);

        if needs_layer {
            let index = self.layers.len();
            let previous_capacity = self
                .layers
                .last()
                .map(|layer| layer.capacity)
                .unwrap_or(self.initial_capacity);
            let capacity = previous_capacity
                .checked_mul(DEFAULT_GROWTH_FACTOR)
                .ok_or_else(|| QuickDbError::new("quickDB scalable-bloom capacity overflow"))?;
            let rate = self.target_false_positive_rate
                * (1.0 - DEFAULT_TIGHTENING_RATIO)
                * DEFAULT_TIGHTENING_RATIO.powi(index as i32);
            self.layers.push(BloomFilter::new(capacity, rate)?);
        }

        if let Some(layer) = self.layers.last_mut() {
            layer.add(value);
        }
        Ok(())
    }

    fn might_contain(&self, value: &[u8]) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.might_contain(value))
    }

    fn clear(&mut self) {
        if let Some(first) = self.layers.first_mut() {
            first.clear();
            self.layers.truncate(1);
        }
    }

    fn stats(&self) -> FilterStats {
        let capacity = self
            .layers
            .iter()
            .fold(0usize, |total, layer| total.saturating_add(layer.capacity));
        let writes = self
            .layers
            .iter()
            .fold(0u64, |total, layer| total.saturating_add(layer.writes));
        let bit_slots = self
            .layers
            .iter()
            .fold(0usize, |total, layer| total.saturating_add(layer.bit_len));
        let allocated_bytes = self.layers.iter().fold(0usize, |total, layer| {
            total.saturating_add(
                layer
                    .words
                    .len()
                    .saturating_mul(std::mem::size_of::<u64>()),
            )
        });
        let hash_functions = self
            .layers
            .iter()
            .map(|layer| layer.hash_functions)
            .max()
            .unwrap_or(0);

        FilterStats {
            kind: FilterKind::ScalableBloom,
            capacity,
            writes,
            bit_slots,
            allocated_bytes,
            hash_functions,
            layers: self.layers.len(),
            target_false_positive_rate: self.target_false_positive_rate,
        }
    }
}

fn bloom_shape(capacity: usize, false_positive_rate: f64) -> Result<(usize, u32), QuickDbError> {
    if capacity == 0 {
        return Err(QuickDbError::new("quickDB capacity must be greater than zero"));
    }
    if !false_positive_rate.is_finite()
        || false_positive_rate <= 0.0
        || false_positive_rate >= 1.0
    {
        return Err(QuickDbError::new(
            "quickDB falsePositiveRate must be between 0 and 1",
        ));
    }

    let bits = (-(capacity as f64) * false_positive_rate.ln() / (LN_2 * LN_2)).ceil();
    if !bits.is_finite() || bits <= 0.0 || bits > usize::MAX as f64 {
        return Err(QuickDbError::new(
            "quickDB filter allocation exceeds this platform's address space",
        ));
    }
    let bit_len = bits as usize;
    let hashes = (((bit_len as f64 / capacity as f64) * LN_2).round() as u32)
        .clamp(1, MAX_HASH_FUNCTIONS);
    Ok((bit_len.max(64), hashes))
}

fn hash_indexes(
    first: u64,
    second: u64,
    hash_functions: u32,
    modulo: usize,
) -> impl Iterator<Item = usize> {
    (0..hash_functions).map(move |index| {
        (first
            .wrapping_add((index as u64).wrapping_mul(second))
            % modulo as u64) as usize
    })
}

fn hash_pair(value: &[u8]) -> (u64, u64) {
    let first = seeded_hash(value, 0x243f_6a88_85a3_08d3);
    let mut second = seeded_hash(value, 0x1319_8a2e_0370_7344);
    second |= 1;
    if second == 0 {
        second = 0x9e37_79b9_7f4a_7c15;
    }
    (first, second)
}

fn seeded_hash(value: &[u8], seed: u64) -> u64 {
    let mut hash = seed ^ 0xcbf2_9ce4_8422_2325;
    for &byte in value {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    avalanche(hash ^ value.len() as u64)
}

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^ (value >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(kind: FilterKind, capacity: usize) -> FilterConfig {
        FilterConfig {
            kind,
            capacity,
            false_positive_rate: 0.01,
        }
    }

    #[test]
    fn filter_kind_parser_accepts_public_aliases() {
        assert_eq!(FilterKind::parse("counting").unwrap(), FilterKind::CountingBloom);
        assert_eq!(FilterKind::parse("countingBloom").unwrap(), FilterKind::CountingBloom);
        assert_eq!(FilterKind::parse("scalable").unwrap(), FilterKind::ScalableBloom);
        assert_eq!(FilterKind::parse("scalableBloom").unwrap(), FilterKind::ScalableBloom);
    }

    #[test]
    fn bloom_has_no_false_negative_for_inserted_values() {
        let db = QuickDb::default();
        db.create("users", config(FilterKind::Bloom, 10_000))
            .unwrap();
        for index in 0..5_000 {
            db.add("users", &format!("user-{index}")).unwrap();
        }
        db.seal("users").unwrap();
        for index in 0..5_000 {
            assert!(db
                .might_contain("users", &format!("user-{index}"))
                .unwrap());
        }
        assert!(db.definitely_missing("users", "definitely-not-added").is_ok());
    }

    #[test]
    fn counting_bloom_supports_known_insert_deletion() {
        let db = QuickDb::default();
        db.create("emails", config(FilterKind::CountingBloom, 1_000))
            .unwrap();
        db.add("emails", "kate@example.test").unwrap();
        db.seal("emails").unwrap();
        assert!(db.might_contain("emails", "kate@example.test").unwrap());
        assert!(db.remove("emails", "kate@example.test").unwrap());
        assert!(db.definitely_missing("emails", "kate@example.test").unwrap());
    }

    #[test]
    fn failed_mutation_requires_rebuild_before_reseal() {
        let db = QuickDb::default();
        db.create("emails", config(FilterKind::CountingBloom, 1))
            .unwrap();
        {
            let mut filters = db.filters.write().unwrap();
            let managed = filters.get_mut("emails").unwrap();
            let Filter::CountingBloom(filter) = &mut managed.filter else {
                panic!("expected counting-bloom filter");
            };
            for index in 0..filter.slot_len {
                filter.set_counter(index, 15);
            }
        }
        db.seal("emails").unwrap();

        let error = db.add("emails", "new@example.test").unwrap_err();
        assert!(error.to_string().contains("counter saturated"));
        assert!(!db.is_ready("emails").unwrap());
        let error = db.seal("emails").unwrap_err();
        assert!(error.to_string().contains("clear or rebuild"));

        db.clear("emails").unwrap();
        db.seal("emails").unwrap();
        assert!(db.is_ready("emails").unwrap());
    }

    #[test]
    fn snapshot_helpers_finish_ready_under_one_registry_write() {
        let db = QuickDb::default();
        db.create("users", config(FilterKind::Bloom, 1_000)).unwrap();
        assert_eq!(
            db.load_snapshot("users", ["old-a", "old-b"].into_iter())
                .unwrap(),
            2
        );
        assert!(db.is_ready("users").unwrap());
        assert!(db.might_contain("users", "old-a").unwrap());
        assert!(db
            .load_snapshot("users", ["duplicate"].into_iter())
            .unwrap_err()
            .to_string()
            .contains("unready filter"));

        assert_eq!(
            db.rebuild_snapshot("users", ["new-a", "new-b", "new-c"].into_iter())
                .unwrap(),
            3
        );
        assert!(db.is_ready("users").unwrap());
        assert!(db.might_contain("users", "new-a").unwrap());
        assert_eq!(db.stats("users").unwrap().writes, 3);
    }

    #[test]
    fn normal_bloom_rejects_deletion() {
        let db = QuickDb::default();
        db.create("users", config(FilterKind::Bloom, 100)).unwrap();
        let error = db.remove("users", "kate").unwrap_err();
        assert!(error.to_string().contains("counting-bloom"));
    }

    #[test]
    fn scalable_bloom_grows_layers_without_losing_old_members() {
        let db = QuickDb::default();
        db.create("handles", config(FilterKind::ScalableBloom, 4))
            .unwrap();
        for value in ["a", "b", "c", "d", "e", "f", "g"] {
            db.add("handles", value).unwrap();
        }
        db.seal("handles").unwrap();
        let stats = db.stats("handles").unwrap();
        assert!(stats.layers >= 2);
        for value in ["a", "b", "c", "d", "e", "f", "g"] {
            assert!(db.might_contain("handles", value).unwrap());
        }
    }

    #[test]
    fn stats_report_packed_allocations() {
        let db = QuickDb::default();
        db.create("bloom", config(FilterKind::Bloom, 10_000)).unwrap();
        db.create("counting", config(FilterKind::CountingBloom, 10_000))
            .unwrap();

        let bloom = db.stats("bloom").unwrap();
        let counting = db.stats("counting").unwrap();
        assert!(bloom.allocated_bytes > 0);
        assert!(counting.allocated_bytes > bloom.allocated_bytes);
        assert_eq!(db.len().unwrap(), 2);
    }

    #[test]
    fn membership_checks_require_a_completed_rebuild() {
        let db = QuickDb::default();
        db.create("users", config(FilterKind::Bloom, 100)).unwrap();
        db.add("users", "kate").unwrap();
        let error = db.might_contain("users", "kate").unwrap_err();
        assert!(error.to_string().contains("quickDB.seal"));
        db.seal("users").unwrap();
        assert!(db.might_contain("users", "kate").unwrap());
        db.clear("users").unwrap();
        assert!(!db.is_ready("users").unwrap());
    }

    #[test]
    fn duplicate_filter_names_are_rejected() {
        let db = QuickDb::default();
        db.create("users", config(FilterKind::Bloom, 10)).unwrap();
        assert!(db
            .create("users", config(FilterKind::Bloom, 10))
            .unwrap_err()
            .to_string()
            .contains("already exists"));
    }
}
