# Built-in profanity lists

> **Summary:** `profanity-{en,es,fr,de}.txt` are the LDNOOBW word lists, unmodified, used by `multi_core::filter` when `filter.profanity = true`. Licence CC-BY-4.0; attribution below. Entries with symbols other than letters, digits, spaces, `'`, `-` (e.g. `s&m`, an emoji) are skipped when loading.

| File | Source file | Entries |
|---|---|---|
| `profanity-en.txt` | `en` | 403 |
| `profanity-es.txt` | `es` | 68 |
| `profanity-fr.txt` | `fr` | 91 |
| `profanity-de.txt` | `de` | 66 |

**Source:** "List of Dirty, Naughty, Obscene, and Otherwise Bad Words" by Shutterstock and contributors, <https://github.com/LDNOOBW/List-of-Dirty-Naughty-Obscene-and-Otherwise-Bad-Words>, commit `5faf2ba42d7b1c0977169ec3611df25a3c08eb13` (fetched 2026-09-25).

**Licence:** Creative Commons Attribution 4.0 International (CC-BY-4.0), <https://creativecommons.org/licenses/by/4.0/>. No changes were made to the files. The attribution must ship with MULTI (e.g. in its third-party notices).

**How the filter uses them:** each caption lane gets its own language's list plus the source language's (a translation can copy a source word unchanged), plus `filter.blocklist`, minus `filter.allowlist`. The lists include some words that are harmless in context (`trio`, `nazi` in `es`, `con` in `fr`); add those to `filter.allowlist` where needed.
