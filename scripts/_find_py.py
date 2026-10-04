#!/usr/bin/env python3
import pathlib, glob
pyg = [d for d in glob.glob('/nix/store/*python3.13-pygobject*') if pathlib.Path(d)/'lib/python3.13/site-packages/gi' in [pathlib.Path(d)/'lib/python3.13/site-packages/gi']]
for d in sorted(glob.glob('/nix/store/*-env')):
    p = pathlib.Path(d)
    if not (p/'bin'/'python3').exists():
        continue
    sips = list(p.glob('lib/python3.13/site-packages/*/'))
    has_pycairo = any(x.name.startswith('pycairo') for x in sips)
    has_gi = any((x/'gi').is_dir() or x.name=='gi' for x in sips)
    if has_pycairo and has_gi:
        print(d)
