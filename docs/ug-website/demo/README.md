# UltraGraph demo — The UltraGraph engine

**Generated. Do not edit by hand.** Every file here is rewritten by:

```bash
ug demo -i <repo> -o <this directory>
```

`index.html` is the same visualization page `ug serve` serves, wrapped in
a static stand-in for the server (`native/src/vis/demo-shim.js`). It reads
`graph.json` from this directory and needs no backend. Everything that
does need one — semantic search, chat, guided tours, statistics, source
preview — is off, and the page says so where a visitor would look.

`demo.json` is the same manifest the page is built with: label, counts,
`ug` version, generation time.
