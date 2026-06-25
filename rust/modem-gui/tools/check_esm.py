#!/usr/bin/env python3
"""Static ESM consistency checker for the (bundler-less) GUI front-end.

For every .js under the given dir:
  - resolve each `import ... from "REL"` path (must exist),
  - verify every NAMED import is actually exported by the target module,
  - flag duplicate top-level definitions of an imported name (stale leftover).

This is the headless safety net for the per-tab module refactor: it catches the
two dominant mechanical errors (missing export, bad path). It does NOT run the
code, so it cannot catch undefined-at-runtime references to a non-imported
global — review + a GUI launch covers that.

Usage: python check_esm.py <src_dir>
"""
import re, sys, os, glob

def exported_names(text):
    names = set()
    for m in re.finditer(r'^export\s+(?:async\s+)?function\s+([A-Za-z0-9_$]+)', text, re.M):
        names.add(m.group(1))
    for m in re.finditer(r'^export\s+(?:const|let|var|class)\s+([A-Za-z0-9_$]+)', text, re.M):
        names.add(m.group(1))
    # export { a, b as c, ... }
    for m in re.finditer(r'^export\s*\{([^}]*)\}', text, re.M):
        for part in m.group(1).split(','):
            part = part.strip()
            if not part:
                continue
            # `a as b` exports b
            alias = re.match(r'[A-Za-z0-9_$]+\s+as\s+([A-Za-z0-9_$]+)', part)
            names.add(alias.group(1) if alias else part)
    if re.search(r'^export\s+default\b', text, re.M):
        names.add('default')
    return names

def imports(text):
    # returns list of (rel_path, [named], has_default, has_namespace, line_no)
    out = []
    for m in re.finditer(r'^import\s+(.*?)\s+from\s+["\']([^"\']+)["\']', text, re.M):
        clause, path = m.group(1), m.group(2)
        line_no = text[:m.start()].count('\n') + 1
        named, has_default, has_ns = [], False, False
        if '*' in clause:
            has_ns = True
        nb = re.search(r'\{([^}]*)\}', clause)
        if nb:
            for part in nb.group(1).split(','):
                part = part.strip()
                if not part:
                    continue
                # `a as b` imports binding a from target
                base = re.match(r'([A-Za-z0-9_$]+)', part)
                if base:
                    named.append(base.group(1))
        # leading default binding (before any `{`)
        head = clause.split('{')[0].strip().rstrip(',').strip()
        if head and not head.startswith('*'):
            has_default = True
        out.append((path, named, has_default, has_ns, line_no))
    return out

def top_level_defs(text):
    names = set()
    for m in re.finditer(r'^(?:export\s+)?(?:async\s+)?function\s+([A-Za-z0-9_$]+)', text, re.M):
        names.add(m.group(1))
    for m in re.finditer(r'^(?:export\s+)?(?:const|let|var|class)\s+([A-Za-z0-9_$]+)', text, re.M):
        names.add(m.group(1))
    return names

def main():
    src = os.path.abspath(sys.argv[1])
    files = glob.glob(os.path.join(src, '**', '*.js'), recursive=True)
    cache = {f: open(f, encoding='utf-8').read() for f in files}
    errors = []
    for f, text in cache.items():
        defs = top_level_defs(text)
        for path, named, has_default, has_ns, line_no in imports(text):
            if not path.startswith('.'):
                continue  # bare specifier (none expected here)
            target = os.path.normpath(os.path.join(os.path.dirname(f), path))
            if not os.path.exists(target):
                errors.append(f"{os.path.relpath(f,src)}:{line_no}  import path not found: {path}")
                continue
            exp = exported_names(cache.get(target, open(target, encoding='utf-8').read()))
            for n in named:
                if n not in exp:
                    errors.append(f"{os.path.relpath(f,src)}:{line_no}  '{n}' not exported by {path}")
                if n in defs:
                    errors.append(f"{os.path.relpath(f,src)}:{line_no}  '{n}' is BOTH imported and locally defined (stale leftover)")
    if errors:
        print("ESM CHECK FAILED:")
        for e in errors:
            print("  " + e)
        sys.exit(1)
    print(f"ESM check OK: {len(files)} module(s), all named imports resolve.")

if __name__ == '__main__':
    main()
