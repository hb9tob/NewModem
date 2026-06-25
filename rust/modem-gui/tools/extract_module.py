#!/usr/bin/env python3
"""Move top-level symbol definitions out of a monolithic ESM file into a new
module, and insert an `import { ... } from "PATH"` back into the source so call
sites stay unchanged.

Relies on this codebase's invariant: every top-level function / const / let is
written at column 0, and a function/object body closes with a `}` (or `};`) at
column 0. Template-literal `${...}` are always balanced, so naive brace counting
is correct for these definitions. The script PRINTS the extracted blocks for
review and refuses to run if a symbol can't be located unambiguously.

Usage:
  python extract_module.py <config.json>

config.json:
  {
    "source": "src/main.js",
    "module_path": "src/lib/format.js",
    "import_from": "./lib/format.js",   # path as written in the source import
    "header": "// Pure formatters ...",
    "symbols": ["MIME_TYPES", "mimeToExt", ...]   # in any order; emitted in file order
  }
"""
import json, re, sys, os

def find_def_start(lines, name):
    # function NAME(  |  const/let/var NAME =  | const NAME = {
    pat = re.compile(
        r'^(?:export\s+)?(?:async\s+)?function\s+' + re.escape(name) + r'\b'
        r'|^(?:export\s+)?(?:const|let|var)\s+' + re.escape(name) + r'\b'
    )
    hits = [i for i, ln in enumerate(lines) if pat.match(ln)]
    if len(hits) != 1:
        raise SystemExit(f"ERROR: symbol {name!r} matched {len(hits)} top-level defs (need exactly 1): lines {[h+1 for h in hits]}")
    return hits[0]

def block_end(lines, start):
    first = lines[start]
    if '{' not in first:
        # single-line const/let (may span if it ends without ';'? assume ';')
        i = start
        while ';' not in lines[i] and i < len(lines) - 1:
            i += 1
        return i
    depth = 0
    started = False
    for i in range(start, len(lines)):
        for ch in lines[i]:
            if ch == '{':
                depth += 1; started = True
            elif ch == '}':
                depth -= 1
        if started and depth == 0:
            return i
    raise SystemExit(f"ERROR: unbalanced braces from line {start+1}")

def main():
    cfg = json.load(open(sys.argv[1], encoding='utf-8'))
    root = os.path.dirname(os.path.abspath(sys.argv[1]))
    src_path = os.path.join(root, cfg['source'])
    lines = open(src_path, encoding='utf-8').read().split('\n')

    spans = []
    for name in cfg['symbols']:
        s = find_def_start(lines, name)
        e = block_end(lines, s)
        spans.append((s, e, name))
    spans.sort()  # file order
    # ensure no overlap
    for (s1, e1, n1), (s2, e2, n2) in zip(spans, spans[1:]):
        if s2 <= e1:
            raise SystemExit(f"ERROR: {n1} (l{s1+1}-{e1+1}) overlaps {n2} (l{s2+1}-{e2+1})")

    # Build module body (emit blocks in file order, prefix `export `).
    out = [cfg['header'], '']
    for s, e, name in spans:
        block = lines[s:e+1]
        if not block[0].startswith('export '):
            block[0] = 'export ' + block[0]
        out.extend(block)
        out.append('')
    module_text = '\n'.join(out).rstrip('\n') + '\n'

    # Remove blocks from source (back-to-front so indices stay valid).
    removed = 0
    for s, e, name in sorted(spans, reverse=True):
        del lines[s:e+1]
        removed += (e - s + 1)

    # Insert the import after the last top-level import near the top.
    last_import = 0
    for i, ln in enumerate(lines[:60]):
        if re.match(r'^import\b.*\bfrom\b', ln):
            last_import = i
    names = ', '.join(n for _, _, n in spans)
    imp = f'import {{ {names} }} from "{cfg["import_from"]}";'
    lines.insert(last_import + 1, imp)

    module_path = os.path.join(root, cfg['module_path'])
    os.makedirs(os.path.dirname(module_path), exist_ok=True)
    open(module_path, 'w', encoding='utf-8', newline='\n').write(module_text)
    open(src_path, 'w', encoding='utf-8', newline='\n').write('\n'.join(lines))

    print(f"OK: moved {len(spans)} symbols ({removed} lines) -> {cfg['module_path']}")
    print(f"    import inserted after source line {last_import+1}: {imp}")
    for s, e, name in spans:
        print(f"    - {name}")

if __name__ == '__main__':
    main()
