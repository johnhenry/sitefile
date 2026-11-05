---
title: "Markdown that stays safe"
description: Raw HTML and javascript links are neutralized by default.
tags: [security]
---

Sitefile's Markdown renderer has no dependencies and supports the parts you
use every day: headings, *emphasis*, **strong**, ~~strike~~, `code`, lists,
quotes, code blocks, [links](https://example.com) and images.

> Content is data, not code.

Raw HTML such as <script>alert(1)</script> is shown as text, and a
[javascript: link](javascript:alert(1)) loses its link.

1. Write posts in Markdown
2. Run `sitefile build examples/blog`
3. Deploy `dist/`
