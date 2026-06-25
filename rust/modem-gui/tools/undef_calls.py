#!/usr/bin/env python3
"""List function-CALL identifiers in a module that are neither defined locally,
imported, nor known builtins/DOM/Array/String methods. Catches the missing
upward-dependency calls (e.g. a tab calling a not-imported function from another
tab) that check_esm.py can't see. More precise than find_undef.py (calls only).
Usage: python undef_calls.py <file.js>
"""
import re, sys

src = open(sys.argv[1], encoding="utf-8").read()
s = re.sub(r"`(?:\\.|[^`\\])*`", " ", src)
s = re.sub(r'"(?:\\.|[^"\\])*"', " ", s)
s = re.sub(r"'(?:\\.|[^'\\])*'", " ", s)
s = re.sub(r"//[^\n]*", " ", s)
s = re.sub(r"/\*.*?\*/", " ", s, flags=re.S)

defined = set(re.findall(r"(?:function|const|let|var|class)\s+([A-Za-z0-9_$]+)", s))
imported = set()
for m in re.finditer(r"import\s*\{([^}]*)\}", s):
    for x in m.group(1).split(","):
        imported.add(x.strip())
calls = set(m.group(1) for m in re.finditer(r"(?<![.\w$])([A-Za-z_$][A-Za-z0-9_$]*)\s*\(", s))

builtins = set("if for while switch catch return function typeof instanceof new await Promise setTimeout clearTimeout setInterval clearInterval requestAnimationFrame cancelAnimationFrame parseInt parseFloat isNaN isFinite String Number Boolean Array Object Math JSON Map Set WeakMap Date RegExp Error encodeURIComponent decodeURIComponent structuredClone queueMicrotask atob btoa fetch alert confirm prompt".split())
dommeth = set("document window getElementById querySelector querySelectorAll closest matches createElement createElementNS createTextNode addEventListener removeEventListener appendChild insertBefore replaceChild removeChild remove append prepend setAttribute getAttribute removeAttribute hasAttribute toggleAttribute dispatchEvent createEvent preventDefault stopPropagation focus blur click select scrollIntoView getContext getBoundingClientRect setProperty getPropertyValue write writeln contains item namedItem".split())
arrstr = set("map filter find findIndex findLast forEach push pop shift unshift slice splice join split concat includes indexOf lastIndexOf reduce reduceRight some every sort reverse fill flat flatMap keys values entries from of isArray toFixed toPrecision toString toUpperCase toLowerCase trim trimStart trimEnd replace replaceAll padStart padEnd startsWith endsWith repeat charAt charCodeAt codePointAt substring substr at match matchAll test exec has get set delete add clear toLocaleString toLocaleTimeString toLocaleDateString valueOf hasOwnProperty assign freeze keys create normalize bind call apply then catch finally all race resolve reject".split())

unknown = sorted(c for c in calls if c not in defined and c not in imported and c not in builtins and c not in dommeth and c not in arrstr)
if unknown:
    print("UNDEFINED CALLS:", unknown)
    sys.exit(1)
print("OK: no undefined calls.")
