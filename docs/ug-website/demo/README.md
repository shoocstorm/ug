# UltraGraph demo — The UltraGraph engine

**Generated. Do not edit by hand.** Every file here is rewritten by:

```bash
ug demo -i <repo> -o <this directory>
```

`index.html` is the same visualization page `ug serve` serves, wrapped in
a static stand-in for the server (`native/src/vis/demo-shim.js`). It reads
`graph.json` from this directory and needs no backend. Everything that
does need one — semantic search, guided tours, statistics, source
preview — is off, and the page says so where a visitor would look.
The one server feature that runs without a server is here: a visitor
can download a small model into the tab and Answer mode answers from
the published graph. `wllama/` holds that model's runtime.

`demo.json` is the same manifest the page is built with: label, counts,
`ug` version, generation time.
