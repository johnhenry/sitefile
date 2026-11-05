---
title: One template, many pages
tags:
  - sitefile
  - templates
---

`RENDER … FOR post IN posts` renders the same template once per post, and the
destination can use the post's fields:

```
RENDER --escape=html ./templates/post.html TO /posts/${post.slug}/index.html FOR post IN posts
```

Inside a template you get loops and conditionals:

- `${#each posts as post}` … `${/each}` repeats a block
- `${#if post.description}` … `${else}` … `${/if}` picks one
- `${post.content|raw}` inserts the rendered HTML without escaping it

Everything else is escaped, so a title like `Tom & Jerry <3` is safe.
