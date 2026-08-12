# /module

Default location `.module` files resolve against — a sibling of the
compiled backend binary, same as `/api/`.

`:import[module&storage]` inside a `.route` (or, once implemented, a
`.module`) file resolves to `storage.module` in this folder.
`:import["./module/storage"]` written out explicitly means the same
thing; the shorthand is just shorter to type for the common case.

**Not functional yet.** Loading and running `.module` files is
designed but not implemented — see `api/README.md`'s "Planned:
`.module` files" section for the full design and current status.
Files placed here right now won't be loaded by anything; importing
one from a `.route` file parses successfully but errors clearly if
you try to call it.
