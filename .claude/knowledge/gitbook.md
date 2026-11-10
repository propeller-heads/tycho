# Writing GitBook Docs

## Style Rules

- **No passive voice** — hard rule. Rewrite every passive construction.
- If a page is reduced to 2–3 sentences, fold it into a parent or related page. Remove its SUMMARY.md entry too.

## Links

External links must open in a new tab — always use the full anchor tag, never bare markdown:
```html
<a href="https://..." target="_blank" rel="noopener noreferrer">link text</a>
```

GitBook only auto-generates anchors for `##` and `###` headings. For `####`, add one explicitly:
```markdown
#### My Heading <a href="#my-heading" id="my-heading"></a>
```

## GitBook Markup

- **Table column widths**: `<th width="N">` in the HTML header.
- **SUMMARY.md**: controls the sidebar. Adding or removing a page requires a matching SUMMARY.md change.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds). The style rules above are
the source of `fold-stub-doc-pages`; the general doc rules are in `general.md`.

<!-- pieces -->

### fold-stub-doc-pages
Source: `.claude/knowledge/gitbook.md` § Style Rules
Spot: A docs diff that leaves a page with only a few sentences, or removes most of a page's content without merging what is left.
Applies: docs
