from pathlib import Path
import sys

if len(sys.argv) != 2:
    raise SystemExit("usage: fix-field-patch-driver.py <patch-script>")

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")

strict_old = '''    if count != 1:\n        raise SystemExit(f"{label}: expected one anchor, found {count}")\n    file.write_text(text.replace(old, new, 1), encoding="utf-8")\n'''
strict_new = '''    if count == 0:\n        raise SystemExit(f"{label}: expected an anchor, found 0")\n    if count != 1 and label != "Field namespace registry":\n        raise SystemExit(f"{label}: expected one anchor, found {count}")\n    file.write_text(text.replace(old, new, 1), encoding="utf-8")\n'''
if strict_old not in text:
    raise SystemExit("unable to patch staged FieldManager driver strictness")
text = text.replace(strict_old, strict_new, 1)

relc_old = '''replace_once(
    "engine/crates/route-engine/src/relc.rs",
    "use crate::execution_tracker::InvocationTracker;\\n",
    "use crate::execution_tracker::InvocationTracker;\\nuse crate::field_manager::field_logical_candidates;\\n",
    "RELC Field logical resolver import",
)
'''
relc_new = '''replace_once(
    "engine/crates/route-engine/src/relc.rs",
    "use crate::embedded_rel::{extract_embedded_rel, EmbeddedRelError};\\n",
    "use crate::embedded_rel::{extract_embedded_rel, EmbeddedRelError};\\nuse crate::field_manager::field_logical_candidates;\\n",
    "RELC Field logical resolver import",
)
'''
if relc_old not in text:
    raise SystemExit("unable to retarget staged RELC Field import anchor")
text = text.replace(relc_old, relc_new, 1)

path.write_text(text, encoding="utf-8")
