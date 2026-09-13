from pathlib import Path

path = Path('.github/patches/ctr007-worker-crash-circuit.py')
text = path.read_text(encoding='utf-8')

old = '''impl WorkerChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }
}
'''
new = '''impl WorkerChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
'''
if text.count(old) != 1:
    raise SystemExit(f'WorkerChildGuard anchor count={text.count(old)}')
text = text.replace(old, new, 1)

old = '''impl Drop for WorkerChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
'''
new = '''impl Drop for WorkerChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}
'''
if text.count(old) != 1:
    raise SystemExit(f'WorkerChildGuard Drop anchor count={text.count(old)}')
text = text.replace(old, new, 1)

old = '''            {
                let _ = reader.join();
                return Err(WorkerExecutionFailure::cancelled());
            }
            if started.elapsed() >= timeout {
                let _ = reader.join();
                return Err(WorkerExecutionFailure::timed_out(timeout_ms));
            }
'''
new = '''            {
                child.terminate();
                let _ = reader.join();
                return Err(WorkerExecutionFailure::cancelled());
            }
            if started.elapsed() >= timeout {
                child.terminate();
                let _ = reader.join();
                return Err(WorkerExecutionFailure::timed_out(timeout_ms));
            }
'''
if text.count(old) != 1:
    raise SystemExit(f'cancel/timeout teardown anchor count={text.count(old)}')
text = text.replace(old, new, 1)

path.write_text(text, encoding='utf-8')
