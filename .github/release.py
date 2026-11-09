"""Maintainer-only portable release assembly; requires Python 3.11 and Cargo cache."""

import argparse
import hashlib
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.request
import zipfile
from contextlib import contextmanager
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent.parent
LICENSES = ROOT / "licenses"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def require(path, size=None):
    if not path.is_file() or path.stat().st_size == 0:
        raise ValueError(f"Missing or empty required input: {path}")
    if size is not None and path.stat().st_size != size:
        raise ValueError(f"Incomplete input: {path}; expected {size} bytes")
    return path


def checked_file(path, expected):
    return path.is_file() and not path.is_symlink() and digest(path) == expected


def acquire_file(destination, pin, caches=()):
    """Publish only complete, checksum-approved downloads or cached archives."""
    if checked_file(destination, pin["sha256"]):
        return destination
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as stream:
        partial = Path(stream.name)
    try:
        cached = next((p for p in caches if checked_file(p, pin["sha256"])), None)
        if cached:
            shutil.copyfile(cached, partial)
        else:
            if not pin["url"].startswith("https://"):
                raise ValueError("Native downloads require HTTPS")
            with (
                urllib.request.urlopen(pin["url"], timeout=120) as source,
                partial.open("wb") as out,
            ):
                shutil.copyfileobj(source, out)
        if not checked_file(partial, pin["sha256"]):
            raise ValueError(f"Native download SHA-256 mismatch: {destination.name}")
        partial.replace(destination)
    finally:
        partial.unlink(missing_ok=True)
    return destination


def native_files_valid(directory, pin):
    if directory.is_symlink():
        return False
    for artifact in pin["files"]:
        relative = PurePosixPath(artifact["path"])
        if (relative.is_absolute() or ".." in relative.parts
                or "\\" in str(relative) or ":" in str(relative)):
            raise ValueError(f"Invalid pinned native path: {relative}")
        path = directory.joinpath(*relative.parts)
        if not path.resolve().is_relative_to(directory.resolve()):
            return False
        if not checked_file(path, artifact["sha256"]):
            return False
    return True


def extract_native(archive, stage, stem):
    """Accept only regular files/directories inside the expected archive root."""
    with tarfile.open(archive, "r:bz2") as source:
        seen = set()
        for member in source:
            relative = PurePosixPath(member.name)
            if (relative.is_absolute() or ".." in relative.parts
                    or "\\" in member.name or ":" in member.name
                    or not relative.parts or relative.parts[0] != stem
                    or not (member.isfile() or member.isdir())):
                raise ValueError(f"Unsafe native archive member: {member.name}")
            key = relative.as_posix().casefold()
            if key in seen:
                raise ValueError(f"Duplicate native archive member: {member.name}")
            seen.add(key)
            destination = stage.joinpath(*relative.parts)
            if member.isdir():
                destination.mkdir(parents=True, exist_ok=True)
            else:
                destination.parent.mkdir(parents=True, exist_ok=True)
                with source.extractfile(member) as data, destination.open("xb") as output:
                    shutil.copyfileobj(data, output)


def install_native(archive, destination, pin):
    if native_files_valid(destination, pin):
        return
    stage = Path(tempfile.mkdtemp(prefix="stage-", dir=destination.parent))
    backup = stage / "previous"
    extracted = stage / destination.name
    published = False
    try:
        extract_native(archive, stage, destination.name)
        if not native_files_valid(extracted, pin):
            raise ValueError("Extracted native files do not match approved pins")
        try:
            if destination.exists():
                destination.rename(backup)
            extracted.rename(destination)
            published = True
        finally:
            if not published and backup.exists():
                backup.rename(destination)
        shutil.rmtree(stage)
    except BaseException as error:
        raise ValueError(
            f"Native preparation failed; recovery data retained at {stage}: {error}"
        ) from error


@contextmanager
def native_lock(directory):
    """Use a stable OS lock; termination releases it without stale state."""
    import msvcrt

    directory.mkdir(parents=True, exist_ok=True)
    with (directory / "bootstrap.lock").open("a+b") as stream:
        stream.seek(0, os.SEEK_END)
        if not stream.tell():
            stream.write(b"\0")
            stream.flush()
        stream.seek(0)
        try:
            msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
        except OSError as error:
            raise ValueError("Another native preparation owns bootstrap.lock") from error
        try:
            yield
        finally:
            stream.seek(0)
            msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)


def find_fxc():
    sdk = (Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)"))
           / "Windows Kits/10/bin")
    candidates = [p for p in sdk.glob("*/x64/fxc.exe")
                  if re.fullmatch(r"\d+\.\d+\.\d+\.\d+", p.parents[1].name)]
    if not candidates:
        raise ValueError("Install the Windows SDK shader compiler (fxc.exe)")
    return max(candidates, key=lambda p: tuple(map(int, p.parents[1].name.split(".")))).resolve()


def prepare_native(env_file):
    pins = json.loads((ROOT / "model-manifests/native-tools.json").read_text(encoding="utf-8-sig"))
    pin = pins["sherpa"]
    name = pin["archive"]
    if Path(name).name != name or not name.endswith(".tar.bz2"):
        raise ValueError("Invalid pinned native archive name")
    runtime = ROOT / ".tools/native-runtime"
    downloads = ROOT / ".tools/native-downloads"
    fxc = require(find_fxc())
    with native_lock(runtime):
        caches = [ROOT / "target/sherpa-onnx-prebuilt"]
        if os.environ.get("CARGO_TARGET_DIR"):
            caches.append(Path(os.environ["CARGO_TARGET_DIR"]).resolve() / "sherpa-onnx-prebuilt")
        if os.environ.get("SHERPA_ONNX_ARCHIVE_DIR"):
            caches.append(Path(os.environ["SHERPA_ONNX_ARCHIVE_DIR"]).resolve())
        archive = acquire_file(downloads / name, pin, [p / name for p in caches])
        redist = acquire_file(downloads / "vc_redist.x64.exe", pins["vc_redist"])
        verify_vc_redist(redist)
        destination = runtime / name.removesuffix(".tar.bz2")
        install_native(archive, destination, pin)
        values = {
            "SHERPA_ONNX_ARCHIVE_DIR": downloads.resolve(),
            "SHERPA_ONNX_LIB_DIR": (destination / "lib").resolve(),
            "GPUI_FXC_PATH": fxc,
            "CS_TRANSLATOR_VC_REDIST": redist.resolve(),
        }
        if any("\n" in str(value) or "\r" in str(value) for value in values.values()):
            raise ValueError("Native paths cannot contain newlines")
        with Path(env_file).open("a", encoding="utf-8", newline="\n") as stream:
            stream.write("".join(f"{key}={value}\n" for key, value in values.items()))


def pe_imports(path):
    """Read normal and delay-load PE imports without loading executable code."""
    data = path.read_bytes()
    pe = struct.unpack_from("<I", data, 60)[0]
    if data[pe : pe + 4] != b"PE\0\0":
        raise ValueError(f"Not a PE binary: {path}")
    if struct.unpack_from("<H", data, pe + 4)[0] != 0x8664:
        raise ValueError(f"Not an x64 binary: {path}")
    count = struct.unpack_from("<H", data, pe + 6)[0]
    size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    image_base = struct.unpack_from("<Q", data, optional + 24)[0]
    sections = []
    for i in range(count):
        virtual_size, address, raw_size, raw = struct.unpack_from(
            "<IIII", data, optional + size + i * 40 + 8
        )
        sections.append((address, max(virtual_size, raw_size), raw))

    def offset(address):
        for base, length, raw in sections:
            if base <= address < base + length:
                return raw + address - base
        raise ValueError(f"Invalid PE address in {path}")

    result = set()
    for index, stride, name_offset in ((1, 20, 12), (13, 32, 4)):
        address = struct.unpack_from("<I", data, optional + 112 + index * 8)[0]
        if not address:
            continue
        pos = offset(address)
        while any(data[pos : pos + stride]):
            name = struct.unpack_from("<I", data, pos + name_offset)[0]
            if index == 13 and not struct.unpack_from("<I", data, pos)[0] & 1:
                name -= image_base
            start = offset(name)
            result.add(data[start : data.index(0, start)].decode("ascii").lower())
            pos += stride
    return sorted(result)


def copy(source, destination):
    require(source)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def tool_identity(value):
    path = Path(shutil.which(str(value)) or value).resolve()
    require(path)
    return {"path": str(path), "sha256": digest(path)}


def compiler_context(target, cargo_build):
    """Reconstruct only Cargo contexts whose precedence we can establish exactly."""
    if target != "x86_64-pc-windows-msvc":
        raise ValueError("Release provenance supports only x86_64-pc-windows-msvc")
    unsupported = (
        "CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    )
    if any(key in os.environ for key in unsupported):
        raise ValueError("Use direct Rust compiler/flag overrides, not CARGO_BUILD_* overrides")
    config = tomllib.loads((ROOT / ".cargo/config.toml").read_text(encoding="utf-8"))
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    for directory in (*ROOT.parents, cargo_home.parent):
        if directory == cargo_home.parent:
            candidates = (cargo_home / "config", cargo_home / "config.toml")
        else:
            candidates = (directory / ".cargo/config", directory / ".cargo/config.toml")
        if any(path.is_file() for path in candidates):
            raise ValueError("External Cargo configuration is unsupported for release provenance")
    if (ROOT / ".cargo/config").exists():
        raise ValueError("Legacy Cargo config is unsupported for release provenance")
    build = config.get("build", {})
    targets = config.get("target", {})
    if any(key.startswith("cfg(") for key in targets):
        raise ValueError("Conditional Cargo target configuration is unsupported")
    target_config = targets.get(target, {})
    prefix = "CARGO_TARGET_" + target.upper().replace("-", "_")
    context_env = {
        "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS", "RUSTC_LINKER", "RUSTUP_TOOLCHAIN",
        prefix + "_RUSTFLAGS", prefix + "_LINKER",
    }
    if context_env.intersection(config.get("env", {})):
        raise ValueError("Compiler context overrides in Cargo [env] are unsupported")
    if os.environ.get("CARGO_BUILD_TARGET"):
        raise ValueError("CARGO_BUILD_TARGET is unsupported; pass the release target explicitly")
    for key in ("rustc", "rustc-wrapper", "rustc-workspace-wrapper"):
        if key in build:
            raise ValueError(f"Cargo build.{key} is unsupported; use its environment override")
    channel = tomllib.loads(
        (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
    )["toolchain"]["channel"]
    pinned = subprocess.check_output(
        ["rustup", "which", "--toolchain", channel, "rustc"], cwd=ROOT, text=True
    ).strip()
    compiler = os.environ.get("RUSTC") or pinned
    identity = tool_identity(compiler)
    # Cargo can pass the rustup proxy as RUSTC; identify the selected compiler,
    # never certify the proxy's bytes as the compiler.
    proxy = shutil.which("rustc")
    if (
        proxy and identity["path"] == str(Path(proxy).resolve())
        and shutil.which("rustup")
        and digest(Path(proxy)) == digest(Path(shutil.which("rustup")))
    ):
        selected = subprocess.check_output(
            ["rustup", "which", "rustc"], cwd=ROOT, text=True
        ).strip()
        identity = tool_identity(selected)
    identity["version"] = subprocess.check_output(
        [identity["path"], "-vV"], cwd=ROOT, text=True
    ).strip()
    if not cargo_build and not os.environ.get("RUSTC"):
        identity = tool_identity(pinned)
        identity["version"] = subprocess.check_output(
            [identity["path"], "-vV"], cwd=ROOT, text=True
        ).strip()
    if cargo_build:
        encoded = os.environ.get("CARGO_ENCODED_RUSTFLAGS", "")
        linker_override = os.environ.get("RUSTC_LINKER", "")
    else:
        if "CARGO_ENCODED_RUSTFLAGS" in os.environ:
            encoded = os.environ["CARGO_ENCODED_RUSTFLAGS"]
        elif "RUSTFLAGS" in os.environ:
            encoded = "\x1f".join(os.environ["RUSTFLAGS"].split())
        else:
            flags = os.environ.get(prefix + "_RUSTFLAGS")
            if flags is not None:
                flags = flags.split()
            else:
                flags = target_config.get("rustflags", build.get("rustflags", []))
                if isinstance(flags, str):
                    flags = flags.split()
            encoded = "\x1f".join(flags)
        linker_override = os.environ.get(
            prefix + "_LINKER", target_config.get("linker", "")
        )
    flags = encoded.split("\x1f")
    if any("linker=" in flag or "linker-flavor=" in flag for flag in flags):
        raise ValueError("Linker selection through rustflags is unsupported")
    wrappers = {}
    for key in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
        value = os.environ.get(key, "")
        wrappers[key] = tool_identity(value) if value else None
    return {
        "compiler": identity, "encoded_rustflags": encoded,
        "linker_override": linker_override, "wrappers": wrappers,
    }


def build_info(target, profile="release", cargo_build=False):
    """One input contract used at build time and again before publication."""
    paths = set()
    for directory in ("crates", "model-manifests", ".cargo"):
        paths.update(
            p for p in (ROOT / directory).rglob("*")
            if p.is_file() and p.suffix.lower() != ".md"
        )
    paths.update(ROOT / name for name in (
        "Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
        ".github/release.py",
    ))
    inputs = {p.relative_to(ROOT).as_posix(): digest(p) for p in sorted(paths)}
    native_root = os.environ.get("SHERPA_ONNX_LIB_DIR")
    if not native_root:
        raise ValueError("SHERPA_ONNX_LIB_DIR must identify the verified build native distribution")
    native = {}
    for path in sorted(Path(native_root).rglob("*.dll")):
        if path.name in native and native[path.name] != digest(path):
            raise ValueError(f"Ambiguous native DLL: {path.name}")
        native[path.name] = digest(path)
    native_libraries = {p.relative_to(Path(native_root)).as_posix(): digest(p)
                        for p in sorted(Path(native_root).rglob("*.lib"))}
    for name in ("sherpa-onnx-c-api.dll", "sherpa-onnx-cxx-api.dll", "onnxruntime.dll"):
        if name not in native:
            raise ValueError(f"Verified native build input missing: {name}")
    context = compiler_context(target, cargo_build)
    pins = json.loads((ROOT / "model-manifests/native-tools.json").read_text(encoding="utf-8-sig"))
    for artifact in pins["sherpa"]["files"]:
        relative = Path(artifact["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError(f"Invalid pinned native path: {relative}")
        path = require(Path(native_root).parent / relative)
        if digest(path) != artifact["sha256"]:
            raise ValueError(f"Native build input does not match approved pin: {relative}")
    if context["linker_override"]:
        linker = Path(tool_identity(context["linker_override"])["path"])
    elif os.environ.get("VCToolsInstallDir"):
        linker = require(Path(os.environ["VCToolsInstallDir"]) / "bin/Hostx64/x64/link.exe")
    elif (candidate := shutil.which("link.exe")) and re.fullmatch(
        r"\d+\.\d+\.\d+", Path(candidate).resolve().parents[3].name
    ):
        linker = Path(candidate).resolve()
    else:
        vswhere = (
            Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)"))
            / "Microsoft Visual Studio/Installer/vswhere.exe"
        )
        linkers = subprocess.check_output(
            [str(require(vswhere)), "-latest", "-products", "*",
             "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
             "-find", "VC/Tools/MSVC/*/bin/Hostx64/x64/link.exe"], text=True).splitlines()
        if not linkers:
            raise ValueError("Cannot establish MSVC build tools/runtime minimum")
        linker = max(
            (Path(p) for p in linkers),
            key=lambda p: tuple(map(int, p.parents[3].name.split("."))),
        )
    if not re.fullmatch(r"\d+\.\d+\.\d+", linker.parents[3].name):
        raise ValueError("Only an identifiable MSVC tools linker is supported for releases")
    tools_version = linker.parents[3].name + ".0"
    context["linker"] = tool_identity(linker)
    if cargo_build:
        for identity in (
            context["compiler"], context["linker"], *context["wrappers"].values(),
        ):
            if identity:
                print(f"cargo:rerun-if-changed={identity['path']}")
    minimum = max((tools_version, pins["vc_redist"]["minimum_runtime"]),
                  key=lambda version: tuple(map(int, version.split("."))))
    return {"schema_version": 2, "target": target, "profile": profile,
            "compiler_context": context,
            "inputs": inputs, "native_dlls": native, "native_libraries": native_libraries,
            "msvc": {"version": tools_version, "linker_sha256": digest(linker)},
            "minimum_vc_runtime": minimum,
            "compiler_environment": {key: os.environ.get(key, "") for key in
                                     ("CC", "CXX", "CFLAGS", "CXXFLAGS")}}


def verify_build(build):
    executable = require(build / "cs-translator.exe")
    embedded = json.loads(subprocess.check_output(
        [str(executable), "--build-info"], cwd=build, text=True, encoding="utf-8", timeout=30))
    current = build_info("x86_64-pc-windows-msvc")
    if embedded != current:
        raise ValueError(
            "Executable build inputs differ from current "
            "source/lock/manifests/compiler/native inputs; rebuild before packaging"
        )
    for name, expected in embedded["native_dlls"].items():
        if digest(require(build / name)) != expected:
            raise ValueError(f"Native DLL differs from executable build input: {name}")
    redist_minimum = json.loads(
        (ROOT / "model-manifests/native-tools.json").read_text(encoding="utf-8-sig")
    )["vc_redist"]["version"]
    if tuple(map(int, redist_minimum.split("."))) < tuple(
        map(int, embedded["minimum_vc_runtime"].split("."))
    ):
        raise ValueError(
            "Approved redistributable is older than the actual build tools; update its reviewed pin"
        )
    return embedded


def windows_powershell(command):
    # A pwsh -> Python -> powershell chain otherwise inherits Core-only modules.
    environment = {key: value for key, value in os.environ.items()
                   if key.upper() != "PSMODULEPATH"}
    executable = (
        Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    )
    script = (
        "$ErrorActionPreference='Stop'; "
        "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); "
        + command
    )
    try:
        return subprocess.check_output(
            [str(executable), "-NoProfile", "-NonInteractive", "-Command", script],
            env=environment, text=True, encoding="utf-8", stderr=subprocess.PIPE, timeout=60,
        )
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.output or str(error)).strip()
        raise ValueError(f"Windows PowerShell verification failed: {detail}") from error


def verify_vc_redist(installer):
    pin = json.loads(
        (ROOT / "model-manifests/native-tools.json").read_text(encoding="utf-8-sig")
    )["vc_redist"]
    if digest(require(installer)) != pin["sha256"]:
        raise ValueError("VC redistributable SHA-256 is not approved")
    quoted = str(installer).replace("'", "''")
    command = (
        f"$p='{quoted}'; $s=Get-AuthenticodeSignature -LiteralPath $p; "
        "$v=(Get-Item -LiteralPath $p).VersionInfo; "
        "[pscustomobject]@{status=[string]$s.Status;subject=$s.SignerCertificate.Subject;"
        "thumbprint=$s.SignerCertificate.Thumbprint;product=$v.ProductName;"
        "version=$v.ProductVersion}|ConvertTo-Json -Compress"
    )
    actual = json.loads(windows_powershell(command))
    if (actual["status"] != "Valid" or "O=Microsoft Corporation" not in actual["subject"]
            or actual["thumbprint"].lower() != pin["signer_thumbprint"].lower()
            or actual["product"] != pin["product"] or actual["version"] != pin["version"]
            or "(x64)" not in actual["product"]):
        raise ValueError(
            "VC redistributable signature/product/x64/version does not match approved pin"
        )
    return {**pin, "verified_signer": actual}


def spdx_text(identifier):
    pins = json.loads((LICENSES / "spdx-sha256.json").read_text())
    expected = pins[identifier]
    cache = ROOT / ".tools/release-license-cache" / (identifier + ".txt")
    if cache.exists():
        if digest(cache) != expected:
            raise ValueError(f"Corrupt SPDX cache (remove and reacquire): {cache}")
        return cache
    url = ("https://raw.githubusercontent.com/spdx/license-list-data/"
           "d46e94e2c78ceede1cfc63cfa0396472d2798d4c/text/" + identifier + ".txt")
    with urllib.request.urlopen(url, timeout=60) as response:
        payload = response.read()
    if hashlib.sha256(payload).hexdigest() != expected:
        raise ValueError(f"Pinned SPDX checksum mismatch: {identifier}")
    cache.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=cache.parent, delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        temporary.replace(cache)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return cache

def dependency_notices(stage):
    # Include shipped desktop normal/build dependencies, not workspace tests/tools.
    command = [
        "cargo",
        "metadata",
        "--locked",
        "--offline",
        "--format-version",
        "1",
        "--filter-platform",
        "x86_64-pc-windows-msvc",
    ]
    process = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, encoding="utf-8")
    if process.returncode:
        raise ValueError(
            "Cargo source metadata unavailable; populate the maintainer Cargo cache first:\n"
            + process.stderr
        )
    metadata = json.loads(process.stdout)
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root = next(p["id"] for p in metadata["packages"] if p["name"] == "cs-desktop")
    resolved, pending = set(), [root]
    while pending:
        node = pending.pop()
        if node in resolved:
            continue
        resolved.add(node)
        pending.extend(dep["pkg"] for dep in nodes[node]["deps"]
                       if any(kind["kind"] in (None, "build") for kind in dep["dep_kinds"]))
    records = []
    for package in metadata["packages"]:
        if package["id"] not in resolved or not package.get("source"):
            continue
        base = Path(package["manifest_path"]).parent
        target = stage / "licenses/dependencies" / (package["name"] + "-" + package["version"])
        expression = package.get("license")
        notices = []
        # Native vendored dependency notices matter as well as crate-level terms.
        for source in base.rglob("*"):
            relative = source.relative_to(base)
            if (source.is_file()
                    and not any(
                        part.lower() in ("test", "tests", "benches", "examples")
                        for part in relative.parts[:-1]
                    )
                    and source.suffix.lower() not in (".svg", ".png", ".jpg", ".woff", ".ttf")
                    and re.match(
                        r"^(licen[cs]e|copying|notice|copyright|third[-_ ]?party|patents)"
                        r"(?:$|[._ -])",
                        source.name, re.I,
                    )):
                copy(source, target / relative)
                notices.append(relative.as_posix())
        if package.get("license_file"):
            source = base / package["license_file"]
            copy(source, target / "DECLARED-LICENSE.txt")
            notices.append("DECLARED-LICENSE.txt")
        if not expression and not notices:
            raise ValueError(f"No redistribution license metadata for {package['id']}")
        # Supply standard texts too when crates only declare an SPDX expression.
        for identifier in sorted(
            set(re.findall(r"[A-Za-z0-9][A-Za-z0-9.+-]*", expression or "")) - {"AND", "OR", "WITH"}
        ):
            cache = spdx_text(identifier)
            copy(cache, stage / "licenses/spdx" / cache.name)
        records.append(
            {
                "name": package["name"],
                "version": package["version"],
                "source": package["source"],
                "license": expression,
                "repository": package.get("repository"),
                "notices": notices,
            }
        )
    (stage / "licenses/dependencies.json").write_text(
        json.dumps(records, indent=2) + "\n", encoding="utf-8"
    )


def assemble(args, stage):
    build = Path(args.build_directory).resolve()
    embedded = verify_build(build)
    catalog = json.loads((ROOT / "model-manifests/catalog.json").read_text(encoding="utf-8-sig"))
    asr = next(item for item in catalog["asr"] if item["id"] == catalog["default_asr"])
    mt = catalog["translation"]
    provenance = {
        "build": embedded,
        "asr": asr,
        "translation": mt,
        "language_identification": catalog["language_identification"],
        "native_imports": {},
        "runtime_prerequisite": (
            "Microsoft Visual C++ v14 x64 Redistributable, same or newer than build tools; "
            "Windows 11 x64"
        ),
    }
    # Only pinned, small acquisition metadata ships; the app installs weights on
    # explicit first-run download. No local model directory is consulted.
    for name in (
        "catalog.json", "nemotron-multilingual.toml", "translation.toml", "native-tools.json",
    ):
        source = require(ROOT / "model-manifests" / name)
        if source.stat().st_size > 1024 * 1024:
            raise ValueError(f"Model metadata unexpectedly exceeds 1 MiB: {source}")
        if source.suffix == ".json":
            json.loads(source.read_text(encoding="utf-8-sig"))
        copy(source, stage / "model-manifests" / name)
    provenance["model_payloads_bundled"] = False
    provenance["first_run_downloads"] = "model-manifests/catalog.json"
    for name in (
        "cs-translator.exe",
        "sherpa-onnx-c-api.dll",
        "sherpa-onnx-cxx-api.dll",
        "onnxruntime.dll",
        "onnxruntime_providers_shared.dll",
    ):
        copy(build / name, stage / name)
    runtime_path = str(build / "onnxruntime.dll").replace("'", "''")
    version_command = f"(Get-Item -LiteralPath '{runtime_path}').VersionInfo.ProductVersion"
    runtime_version = windows_powershell(version_command).strip()
    if runtime_version != "1.28.2":
        raise ValueError(
            f"ASR ONNX Runtime {runtime_version!r} is not covered by pinned 1.28.2 notices; "
            "update license sources for the actual build"
        )
    provenance["native_runtime_versions"] = {
        "ASR ONNX Runtime": runtime_version,
        "sherpa-onnx native distribution": json.loads(
            (ROOT / "model-manifests/native-tools.json").read_text(encoding="utf-8-sig")
        )["sherpa"]["version"],
    }
    bundled = {p.name.lower() for p in stage.rglob("*.dll")}
    os_dlls = {
        "kernel32.dll",
        "advapi32.dll",
        "bcryptprimitives.dll",
        "oleaut32.dll",
        "combase.dll",
        "ole32.dll",
        "user32.dll",
        "userenv.dll",
        # gpui-pre-windows uses AccessKit's native UI Automation provider.
        "uiautomationcore.dll",
        "coremessaging.dll",
        "d3d11.dll",
        "dwmapi.dll",
        "gdi32.dll",
        "gdiplus.dll",
        "shell32.dll",
        "dxgi.dll",
        "ntdll.dll",
        "dwrite.dll",
        "comctl32.dll",
        "imm32.dll",
        "dcomp.dll",
        "icuuc.dll",
        "mmdevapi.dll",
        "dbghelp.dll",
        "setupapi.dll",
        "ws2_32.dll",
        "bcrypt.dll",
        "version.dll",
        "shlwapi.dll",
        "ucrtbase.dll",
        "rpcrt4.dll",
        "secur32.dll",
        "winmm.dll",
        "winhttp.dll",
        "crypt32.dll",
        "powrprof.dll",
        "propsys.dll",
    }
    for binary in list(stage.rglob("*.dll")) + [stage / "cs-translator.exe"]:
        imports = pe_imports(binary)
        provenance["native_imports"][binary.relative_to(stage).as_posix()] = imports
        for name in imports:
            if (
                name in bundled
                or name in os_dlls
                or name.startswith(
                    ("api-ms-win-", "ext-ms-win-", "vcruntime140", "msvcp140", "vcomp140")
                )
            ):
                continue
            raise ValueError(
                f"Unaccounted native dependency {name} imported by {binary.name}; "
                "do not copy SDK tools as runtimes"
            )
    for record in json.loads((LICENSES / "sources.json").read_text()):
        source = require(LICENSES / record["file"])
        if digest(source) != record["sha256"]:
            raise ValueError(f"Pinned license checksum mismatch: {source}")
        copy(source, stage / "licenses" / source.name)
    copy(LICENSES / "sources.json", stage / "licenses/sources.json")
    copy(LICENSES / "ATTRIBUTION.txt", stage / "THIRD-PARTY-NOTICES.txt")
    copy(
        ROOT / "crates/desktop/assets/flags/LICENSE",
        stage / "licenses/flag-icons-MIT.txt",
    )
    provenance["flag_artwork"] = {
        "source": "https://github.com/lipis/flag-icons/tree/v7.6.0",
        "license": "MIT",
        "changes": "SVG intrinsic size set to 24x18 for compact rasterization",
    }
    dependency_notices(stage)
    copy(ROOT / "Cargo.lock", stage / "provenance/Cargo.lock")
    (stage / "provenance/selected-asr-files.json").write_text(
        json.dumps(asr["files"], indent=2) + "\n", encoding="utf-8"
    )
    (stage / "provenance/build-info.json").write_text(
        json.dumps(embedded, indent=2) + "\n", encoding="utf-8"
    )
    if args.vc_redist:
        installer = require(Path(args.vc_redist).resolve())
        provenance["vc_redist"] = verify_vc_redist(installer)
        copy(installer, stage / "prerequisites/vc_redist.x64.exe")
    copy(ROOT / "README.md", stage / "README.md")
    inventory = []
    for path in sorted(stage.rglob("*")):
        if path.is_file():
            inventory.append(
                {
                    "file": path.relative_to(stage).as_posix(),
                    "bytes": path.stat().st_size,
                    "sha256": digest(path),
                }
            )
    provenance["files"] = inventory
    provenance["total_payload_bytes"] = sum(item["bytes"] for item in inventory)
    (stage / "provenance/payload.json").write_text(
        json.dumps(provenance, indent=2) + "\n", encoding="utf-8"
    )
    return provenance["total_payload_bytes"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination")
    parser.add_argument("--build-directory")
    parser.add_argument("--emit-build-info")
    parser.add_argument("--target", default="x86_64-pc-windows-msvc")
    parser.add_argument("--profile", default="release")
    parser.add_argument("--cargo-build-context", action="store_true")
    parser.add_argument("--vc-redist")
    parser.add_argument("--prepare-native", action="store_true")
    parser.add_argument("--github-env")
    args = parser.parse_args()
    if args.prepare_native:
        if not args.github_env or args.emit_build_info or args.destination or args.build_directory:
            parser.error(
                "--prepare-native requires --github-env and cannot package or emit build info"
            )
        prepare_native(args.github_env)
        return
    if args.github_env:
        parser.error("--github-env requires --prepare-native")
    if args.emit_build_info:
        Path(args.emit_build_info).write_text(
            json.dumps(
                build_info(args.target, args.profile, args.cargo_build_context),
                sort_keys=True,
            ) + "\n",
            encoding="utf-8",
        )
        return
    if not args.destination or not args.build_directory:
        parser.error("--destination and --build-directory are required for packaging")
    destination = Path(args.destination).resolve()
    archive = destination.with_name(destination.name + ".zip")
    if destination.exists() or archive.exists():
        raise ValueError(
            f"Refusing to overwrite existing release: {destination} or {archive}; "
            "choose a new destination"
        )
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{destination.name}.staging-", dir=destination.parent))
    tree = staging / destination.name
    tree.mkdir()
    temporary_zip = staging / archive.name
    published_tree = False
    try:
        size = assemble(args, tree)
        with zipfile.ZipFile(temporary_zip, "w", compression=zipfile.ZIP_DEFLATED,
                             compresslevel=6, allowZip64=True, strict_timestamps=False) as output:
            for path in sorted(tree.rglob("*")):
                if path.is_file():
                    output.write(path, path.relative_to(staging).as_posix())
        if destination.exists() or archive.exists():
            raise ValueError("Release destination appeared during staging; refusing overwrite")
        for attempt in range(5):
            try:
                tree.rename(destination)
                published_tree = True
                break
            except PermissionError:
                if attempt == 4:
                    raise
                time.sleep(0.5 * (attempt + 1))
        try:
            # Hard-link publication refuses existing destinations, unlike replace().
            os.link(temporary_zip, archive)
        except Exception:
            destination.rename(tree)
            published_tree = False
            raise
        try:
            shutil.rmtree(staging)
        except OSError as cleanup_error:
            print(
                f"Release published; temporary archive retained at {staging}: {cleanup_error}",
                file=sys.stderr,
            )
    except Exception as error:
        location = destination if published_tree else staging
        raise ValueError(
            f"Release publication failed; retained failed staging at {location}: {error}"
        ) from error
    print(
        f"Created {destination}\nCreated {archive}\n"
        f"Uncompressed payload: {size:,} bytes (plus provenance inventory)."
    )


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, StopIteration, subprocess.SubprocessError) as error:
        print(f"Packaging error: {error}", file=sys.stderr)
        sys.exit(1)
