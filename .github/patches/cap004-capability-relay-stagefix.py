from pathlib import Path

path = Path('.github/patches/cap004-capability-relay.py')
text = path.read_text(encoding='utf-8')

old_anchor = '''    cancelled: Mutex<HashMap<String, Instant>>,\n}'''
new_anchor = '''    executions: Mutex<HashMap<String, ExecutionOwner>>,\n    cancelled: Mutex<HashMap<String, Instant>>,\n}'''
old_replacement = '''    cancelled: Mutex<HashMap<String, Instant>>,\n    capability_broker: Arc<CapabilityBroker>,\n    capability_dispatcher: CapabilityDispatcher,\n}'''
new_replacement = '''    executions: Mutex<HashMap<String, ExecutionOwner>>,\n    cancelled: Mutex<HashMap<String, Instant>>,\n    capability_broker: Arc<CapabilityBroker>,\n    capability_dispatcher: CapabilityDispatcher,\n}'''
if text.count(old_anchor) != 1 or text.count(old_replacement) != 1:
    raise SystemExit('CAP-004 supervisor field patch anchor changed')
text = text.replace(old_anchor, new_anchor, 1).replace(old_replacement, new_replacement, 1)

# Rust parses `as usize <` ambiguously in this expression; keep the cast
# explicitly parenthesized in the generated execution-engine patch.
old_compare = 'if capacity as usize < response.len() {'
new_compare = 'if (capacity as usize) < response.len() {'
if text.count(old_compare) != 1:
    raise SystemExit(f'expected one CAP-004 capacity comparison, found {text.count(old_compare)}')
text = text.replace(old_compare, new_compare, 1)

path.write_text(text, encoding='utf-8')
