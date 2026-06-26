# Wiki source

These Markdown files are the source for the project's **GitHub Wiki**
(`Home.md`, `FAQ.md`). They are kept here so the wiki content is version-controlled
and reviewable; the wiki itself is a curated **hub** that links back to the
authoritative, in-repo docs (it deliberately does not duplicate them).

## Publishing

A GitHub wiki's git repository (`<repo>.wiki.git`) is not created until the **first
page is made through the web UI**, so it cannot be bootstrapped by `git push`
alone. One-time setup:

1. On GitHub, open the repo's **Wiki** tab and click **Create the first page**
   (any content) and save — this creates `quiver.wiki.git`.
2. Then publish these pages:

   ```bash
   git clone https://github.com/achref-soua/quiver.wiki.git
   cp docs/wiki/Home.md docs/wiki/FAQ.md quiver.wiki/
   cd quiver.wiki && git add -A && git commit -m "docs(wiki): home hub + faq" && git push
   ```

Wiki-relative links (e.g. `[FAQ](FAQ)`) resolve on the wiki; on GitHub's file
view here they will not — that is expected, since these are the wiki's source.
