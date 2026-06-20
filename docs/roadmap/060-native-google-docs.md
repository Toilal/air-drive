# 060 — Native Google Docs handling

- **Priority:** 🟡 medium
- **Status:** Planned
- **Issue:** [#3](https://github.com/Toilal/air-drive/issues/3)
- **Area:** reconcile, sync

## Goal

Give native Google formats (Docs, Sheets, Slides) a defined, useful behaviour
instead of being silently invisible.

## Today

Items with the `application/vnd.google-apps.*` mime prefix have no md5 and can't
be synced as opaque bytes, so the reconciler skips them silently (see
[sync model](../dev/sync-model.md#native-google-docs)). Users get no local trace
of these files.

## Approach (to decide)

One of, or a combination:

- **Export** to a concrete format (e.g. `.docx`/`.xlsx`/`.pdf`) on download.
- Write a **`.gdoc`-style shortcut** file that opens the doc in the browser.
- **Skip explicitly with UX** — surface the skip in `status` rather than hiding
  it.

The decision and its rationale should be documented when made.

## Acceptance

- Native docs have a documented, predictable outcome.
- `status` makes any skipped/native items visible.
