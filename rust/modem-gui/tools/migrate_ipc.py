#!/usr/bin/env python3
"""Step 2: route every window.__TAURI__ CALL in main.js through lib/ipc.js.

Removes the per-function `const { invoke } = window.__TAURI__.core;` destructures
(invoke/listen/convertFileSrc are now imported), rewrites the few direct call
expressions, and inserts the import. Defensive `if (!window.__TAURI__) ...`
guards are intentionally left (read-only availability checks).
"""
import re, os

SRC = os.path.join(os.path.dirname(__file__), "..", "src", "main.js")
text = open(SRC, encoding="utf-8").read()
orig = text

# 1. Drop the local destructures (all destructure only invoke/listen/convertFileSrc).
text, n_destr = re.subn(
    r'^[ \t]*const \{[^}]*\} = window\.__TAURI__\.(?:core|event);[ \t]*\n',
    '', text, flags=re.M)

# 2. The optional-opener block -> wrapper.
opener_block = (
    "      if (window.__TAURI__.opener && window.__TAURI__.opener.openUrl) {\n"
    "        await window.__TAURI__.opener.openUrl(link);\n"
    "      }"
)
assert opener_block in text, "opener block not found verbatim"
text = text.replace(opener_block, "      await openExternalUrl(link);")

# 3. Direct call expressions -> imported wrappers.
reps = {
    "window.__TAURI__.core.invoke(": "invoke(",
    "window.__TAURI__.event.listen(": "listen(",
    "window.__TAURI__.window.getCurrentWindow(": "getCurrentWindow(",
}
counts = {}
for a, b in reps.items():
    counts[a] = text.count(a)
    text = text.replace(a, b)

# 4. Insert the import after the last top-level import near the top.
lines = text.split("\n")
last_import = 0
for i, ln in enumerate(lines[:60]):
    if re.match(r'^import\b.*\bfrom\b', ln):
        last_import = i
imp = 'import { invoke, listen, convertFileSrc, getCurrentWindow, openExternalUrl } from "./lib/ipc.js";'
lines.insert(last_import + 1, imp)
text = "\n".join(lines)

open(SRC, "w", encoding="utf-8", newline="\n").write(text)
print(f"OK: removed {n_destr} destructures; rewrote calls:",
      {k.split('.')[-1].rstrip('('): v for k, v in counts.items()})
# Residual direct global CALLS must be gone (guards remain).
resid = re.findall(r'window\.__TAURI__\.(core|event|window|opener)\.[a-zA-Z]+\(', text)
print("residual direct __TAURI__ calls in main.js:", len(resid), resid[:6])
