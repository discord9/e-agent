# Vendored frontend libraries

The Web UI is assembled as a self-contained page and must work offline, so these
pinned browser distributions are committed verbatim rather than loaded from a
CDN. Source maps are intentionally omitted.

| File | Project | Version | License | Upstream distribution |
| --- | --- | ---: | --- | --- |
| `marked.min.js` | marked | 12.0.2 | MIT | <https://cdn.jsdelivr.net/npm/marked@12.0.2/marked.min.js> |
| `katex.min.css` | KaTeX | 0.16.10 | MIT | <https://cdn.jsdelivr.net/npm/katex@0.16.10/dist/katex.min.css> |
| `uPlot.iife.min.js`, `uPlot.min.css` | uPlot | 1.6.31 | MIT | <https://cdn.jsdelivr.net/npm/uplot@1.6.31/dist/> |
| `gridjs.umd.js`, `gridjs.mermaid.min.css` | Grid.js | 6.2.0 | MIT | <https://cdn.jsdelivr.net/npm/gridjs@6.2.0/dist/> |

SHA-256 checksums (exact vendored bytes):

| File | SHA-256 |
| --- | --- |
| `uPlot.iife.min.js` | `2d27e8ad3d228164525ce213f9dc716f39b4e3aee0cc773fb3491c96cf4921a2` |
| `uPlot.min.css` | `df630c6a8d6f8eeaff264b50f73ce5b114f646ffd9a0bb74f049b0a00135fa04` |
| `gridjs.umd.js` | `f7402f347715568c73f061781edd8e7dceeecdd7e2503c28a1012b7ccbc12509` |
| `gridjs.mermaid.min.css` | `ab9585e3983a57267a8f22f708fe40ad70f8c1bd5688ebfba31d11a0c7cca331` |

License texts and source repositories:

- marked: <https://github.com/markedjs/marked/blob/v12.0.2/LICENSE.md>
- KaTeX: <https://github.com/KaTeX/KaTeX/blob/v0.16.10/LICENSE>
- uPlot: <https://github.com/leeoniya/uPlot/blob/1.6.31/LICENSE>; local copy: [`LICENSE.uPlot`](LICENSE.uPlot)
- Grid.js: <https://github.com/grid-js/gridjs/blob/v6.2.0/LICENSE>; local copy: [`LICENSE.Grid.js`](LICENSE.Grid.js)
