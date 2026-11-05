---
title: Why WebAssembly plugins
---
A WebAssembly plugin is a module with no imports. It can't open files, reach the
network or read the clock, so it can only turn its input into output. That makes it
safe to run, and its results are reproducible and cacheable by construction.
