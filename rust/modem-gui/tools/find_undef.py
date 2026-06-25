#!/usr/bin/env python3
# Heuristic: list identifiers referenced in a module that are neither defined
# locally, imported, JS keywords, nor common globals. Surfaces missing globals
# (e.g. constants left behind during extraction). Has false positives (object
# shorthand, destructuring) but reliably catches undefined top-level refs.
import re, sys
src = open(sys.argv[1], encoding="utf-8").read()
nostr = re.sub(r'`(?:\\.|[^`\\])*`', ' ', src)
nostr = re.sub(r'"(?:\\.|[^"\\])*"', ' ', nostr)
nostr = re.sub(r"'(?:\\.|[^'\\])*'", ' ', nostr)
nostr = re.sub(r'//[^\n]*', ' ', nostr)
nostr = re.sub(r'/\*.*?\*/', ' ', nostr, flags=re.S)
defined = set(re.findall(r'(?:function|const|let|var)\s+([A-Za-z0-9_$]+)', nostr))
for m in re.finditer(r'function\s*[A-Za-z0-9_$]*\s*\(([^)]*)\)', nostr):
    for p in m.group(1).split(','):
        p = p.strip().split('=')[0].strip().lstrip('.')
        if p:
            defined.add(p)
for m in re.finditer(r'\(?\b([A-Za-z0-9_$,\s]*)\)?\s*=>', nostr):
    for p in m.group(1).split(','):
        p = p.strip().split('=')[0].strip()
        if p:
            defined.add(p)
imported = set()
for m in re.finditer(r'import\s*\{([^}]*)\}', nostr):
    for x in m.group(1).split(','):
        imported.add(x.strip())
idents = set(m.group(1) for m in re.finditer(r'(?<![.\w$])([A-Za-z_$][A-Za-z0-9_$]*)', nostr))
kw = set("if else for while switch case default break continue return function const let var new typeof instanceof of in this true false null undefined void delete try catch finally throw async await export import from do yield class extends super static get set".split())
glob = set("document window console Math Number String Object Array JSON Boolean Map Set Promise Date RegExp parseInt parseFloat isNaN isFinite setTimeout clearTimeout setInterval clearInterval requestAnimationFrame Event CustomEvent HTMLElement HTMLInputElement HTMLSelectElement HTMLOptionElement Error localStorage navigator alert".split())
suspect = sorted(i for i in idents if i not in defined and i not in imported and i not in kw and i not in glob)
# Focus on the ones that look like module-level names (CONST_CASE or known prefixes)
print("ALL suspects:", suspect)
print("CONST_CASE/likely-global suspects:", [s for s in suspect if re.match(r'^[A-Z][A-Z0-9_]+$', s) or s[0].islower() and any(c.isupper() for c in s)])
