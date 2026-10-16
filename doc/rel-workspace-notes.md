# Compatibility notes

Existing `$$/` ProjectRoot references remain valid. New APIs should prefer explicit `workspace` helpers where that improves readability, but `script` and `archive` continue accepting symbolic path strings so old/new styles can coexist.

`??/` is the symbolic spelling for the execution-scoped workspace root. It is intentionally global so package/build/deployment code can share one path vocabulary across REL, Container and RPX.
