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
