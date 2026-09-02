#!/usr/bin/env python3
import argparse, hashlib, json, pathlib, shutil, tempfile, zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
EXCLUDED_NAMES = {
    'target', 'gen', '__pycache__', '.pytest_cache', 'webview2-realm-disable-results.json',
    'webview2-realm-disable-run.log', 'measurement-config.json',
    'loopback-endpoints.json', 'offhost-endpoints.json', 'sink-request.json',
    'sink-local.stdout.log', 'sink-local.stderr.log'
}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_files():
    for path in sorted(ROOT.rglob('*')):
        if not path.is_file():
            continue
        rel = path.relative_to(ROOT)
        if any(part in EXCLUDED_NAMES for part in rel.parts) or path.suffix == '.zip':
            continue
        yield path, rel


def build(output):
    output.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path, rel in source_files():
            name = pathlib.PurePosixPath('buzz-webview2-realm-disable-harness') / rel.as_posix()
            info = zipfile.ZipInfo(str(name), (1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (0o100755 if path.stat().st_mode & 0o111 else 0o100444) << 16
            info.create_system = 3
            archive.writestr(info, path.read_bytes(), compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    output = pathlib.Path(args.output)
    twin = output.with_suffix('.second.zip')
    build(output); build(twin)
    first, second = sha256(output), sha256(twin)
    if first != second:
        raise SystemExit('deterministic archive mismatch')
    twin.unlink()
    print(json.dumps({'result':'PASS','archive':str(output),'sha256':first,'files':len(zipfile.ZipFile(output).namelist())},sort_keys=True))


if __name__ == '__main__':
    main()
