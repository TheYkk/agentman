#!/usr/bin/env python3
"""
Check whether the version pins in docker-bake.hcl are up-to-date.

Primary inputs: `docker-bake.hcl` variables (notably lines ~14–25).

Examples:
  python3 scripts/check-bake-versions.py
  python3 scripts/check-bake-versions.py --fail
  python3 scripts/check-bake-versions.py --print-vars --only IMAGE_NAME IMAGE_TAG
  python3 scripts/check-bake-versions.py --update
  python3 scripts/check-bake-versions.py --update --dry-run
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Iterable, Literal


BAKE_DEFAULT = os.path.join(os.path.dirname(os.path.dirname(__file__)), "docker-bake.hcl")


def _http_get_text(url: str, *, headers: dict[str, str] | None = None, timeout_s: int = 20) -> str:
    req = urllib.request.Request(
        url,
        headers={
            "User-Agent": "agentman-version-checker",
            **(headers or {}),
        },
        method="GET",
    )
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        raw = resp.read()
    return raw.decode("utf-8", errors="replace")


def _http_get_json(url: str, *, headers: dict[str, str] | None = None, timeout_s: int = 20) -> object:
    text = _http_get_text(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            **(headers or {}),
        },
        timeout_s=timeout_s,
    )
    return json.loads(text)


_VAR_RE = re.compile(
    r'variable\s+"(?P<name>[^"]+)"\s*\{\s*default\s*=\s*"(?P<value>[^"]*)"\s*\}',
    flags=re.MULTILINE,
)


def parse_bake_variables(hcl_text: str) -> dict[str, str]:
    return {m.group("name"): m.group("value") for m in _VAR_RE.finditer(hcl_text)}


def _parse_ints(version: str) -> tuple[int, ...] | None:
    """
    Parse simple dotted numeric versions into a tuple of ints.
    Returns None if the string isn't a simple numeric dotted version.
    """
    if not re.fullmatch(r"\d+(?:\.\d+)*", version):
        return None
    return tuple(int(p) for p in version.split("."))


def _strip_prefixes(s: str, prefixes: tuple[str, ...]) -> str:
    for p in prefixes:
        if s.startswith(p):
            return s[len(p) :]
    return s


def _github_latest_tag(owner: str, repo: str) -> str:
    """
    Default strategy: GitHub Releases "latest".
    Some repos (e.g. rust-lang/rustup) may not publish GitHub Releases; for those use the Tags API.
    """
    data = _http_get_json(f"https://api.github.com/repos/{owner}/{repo}/releases/latest")
    if not isinstance(data, dict) or "tag_name" not in data:
        raise RuntimeError(f"Unexpected GitHub API response for {owner}/{repo}")
    tag = data["tag_name"]
    if not isinstance(tag, str) or not tag.strip():
        raise RuntimeError(f"Unexpected tag_name for {owner}/{repo}: {tag!r}")
    return tag.strip()


def _github_latest_tag_from_tags_api(owner: str, repo: str) -> str:
    data = _http_get_json(f"https://api.github.com/repos/{owner}/{repo}/tags?per_page=100")
    if not isinstance(data, list) or not data:
        raise RuntimeError(f"Unexpected GitHub tags API response for {owner}/{repo}")
    first = data[0]
    if not isinstance(first, dict) or "name" not in first:
        raise RuntimeError(f"Unexpected GitHub tags API response for {owner}/{repo}: {first!r}")
    tag = first["name"]
    if not isinstance(tag, str) or not tag.strip():
        raise RuntimeError(f"Unexpected tag name for {owner}/{repo}: {tag!r}")
    return tag.strip()


def _latest_rust_toolchain() -> str:
    # Prefer tomllib if available; fall back to regex.
    toml_text = _http_get_text("https://static.rust-lang.org/dist/channel-rust-stable.toml")

    try:
        import tomllib  # py>=3.11

        data = tomllib.loads(toml_text)
        version_str = data["pkg"]["rust"]["version"]
        if not isinstance(version_str, str):
            raise RuntimeError("pkg.rust.version is not a string")
        m = re.search(r"(\d+\.\d+\.\d+)", version_str)
        if not m:
            raise RuntimeError(f"Could not parse rust version from: {version_str!r}")
        return m.group(1)
    except Exception:
        # Regex fallback: locate [pkg.rust] section and then its version
        m_section = re.search(r"(?ms)^\[pkg\.rust\]\s*$([\s\S]*?)(^\[|\Z)", toml_text)
        if not m_section:
            raise RuntimeError("Could not find [pkg.rust] section in channel-rust-stable.toml")
        section = m_section.group(1)
        m_ver = re.search(r'(?m)^\s*version\s*=\s*"([^"]+)"\s*$', section)
        if not m_ver:
            raise RuntimeError("Could not find pkg.rust version in channel-rust-stable.toml")
        version_str = m_ver.group(1)
        m = re.search(r"(\d+\.\d+\.\d+)", version_str)
        if not m:
            raise RuntimeError(f"Could not parse rust version from: {version_str!r}")
        return m.group(1)


def _latest_go_version() -> str:
    text = _http_get_text("https://go.dev/VERSION?m=text").strip()
    # Example: "go1.25.5"
    text = text.splitlines()[0].strip()
    if not text.startswith("go"):
        raise RuntimeError(f"Unexpected go VERSION response: {text!r}")
    return text.removeprefix("go")


def _latest_node_patch_for_major(major: int) -> str | None:
    """
    Return the latest Node.js patch release for a given major.

    Uses the official dist index:
      https://nodejs.org/dist/index.json
    """
    data = _http_get_json("https://nodejs.org/dist/index.json")
    if not isinstance(data, list):
        raise RuntimeError("Unexpected Node.js dist index JSON")

    best_key: tuple[int, ...] | None = None
    best: str | None = None
    for item in data:
        if not isinstance(item, dict):
            continue
        v = item.get("version")
        if not isinstance(v, str):
            continue
        v = v.strip()
        if not v.startswith("v"):
            continue
        num = v.removeprefix("v")
        key = _parse_ints(num)
        if key is None or not key:
            continue
        if key[0] != major:
            continue
        if best_key is None or key > best_key:
            best_key = key
            best = num
    return best


def _debian_stable_codename() -> str:
    rel = _http_get_text("https://deb.debian.org/debian/dists/stable/Release")
    m = re.search(r"(?m)^Codename:\s*(\S+)\s*$", rel)
    if not m:
        raise RuntimeError("Could not parse Debian stable codename")
    return m.group(1).strip()


def _latest_python_patch_for_minor(major_minor: str) -> str | None:
    # Official Python FTP index contains directories like "3.13.1/".
    index = _http_get_text("https://www.python.org/ftp/python/")
    versions = []
    for m in re.finditer(r'href="(\d+\.\d+\.\d+)/"', index):
        v = m.group(1)
        if not v.startswith(f"{major_minor}."):
            continue
        key = _parse_ints(v)
        if key is None:
            continue
        versions.append((key, v))
    if not versions:
        return None
    return max(versions, key=lambda kv: kv[0])[1]


def _sdkman_java_versions_table(platform: str = "linuxx64") -> str:
    # Note: requires installed= parameter, even if empty.
    return _http_get_text(
        f"https://api.sdkman.io/2/candidates/java/{platform}/versions/list?installed="
    )


def _latest_sdkman_java_for_identifier(current_identifier: str, *, platform: str = "linuxx64") -> str | None:
    """
    current_identifier examples: "21.0.9-tem", "17.0.17-tem"
    """
    m = re.fullmatch(r"(?P<num>\d+(?:\.\d+)*)(?:-(?P<dist>[A-Za-z0-9]+))?", current_identifier)
    if not m:
        return None
    num = m.group("num")
    dist = m.group("dist") or ""

    num_key = _parse_ints(num)
    if not num_key:
        return None
    major = str(num_key[0])

    table = _sdkman_java_versions_table(platform=platform)
    candidates: list[tuple[tuple[int, ...], str]] = []
    for line in table.splitlines():
        # We only need the Identifier column, which ends the line.
        ident = line.strip().split()[-1] if line.strip() else ""
        if not ident or "|" in ident:
            continue
        if dist and not ident.endswith(f"-{dist}"):
            continue
        # ident like "21.0.9-tem"
        if not ident.startswith(f"{major}."):
            continue
        m_ident = re.fullmatch(r"(?P<num>\d+(?:\.\d+)*)(?:-(?P<dist>[A-Za-z0-9]+))?", ident)
        if not m_ident:
            continue
        ident_num = m_ident.group("num")
        ident_key = _parse_ints(ident_num)
        if not ident_key:
            continue
        candidates.append((ident_key, ident))
    if not candidates:
        return None
    return max(candidates, key=lambda kv: kv[0])[1]


Status = Literal["ok", "outdated", "unknown"]


@dataclass(frozen=True)
class Check:
    name: str
    current: str
    latest: str | None
    status: Status
    source: str
    note: str | None = None


def _check_debian_tag(current: str) -> Check:
    try:
        stable_codename = _debian_stable_codename()
        latest = f"{stable_codename}-slim"
        if current in {"stable", "stable-slim"}:
            return Check("DEBIAN_TAG", current, latest, "ok", "deb.debian.org", "tracks stable")
        status: Status = "ok" if current == latest else "outdated"
        return Check("DEBIAN_TAG", current, latest, status, "deb.debian.org")
    except Exception as e:
        return Check("DEBIAN_TAG", current, None, "unknown", "deb.debian.org", str(e))


def _check_github_version(
    *,
    name: str,
    current: str,
    owner: str,
    repo: str,
    strip: tuple[str, ...] = (),
    add_v: bool = False,
    strategy: Literal["releases", "tags"] = "releases",
) -> Check:
    try:
        tag = (
            _github_latest_tag(owner, repo)
            if strategy == "releases"
            else _github_latest_tag_from_tags_api(owner, repo)
        )
        latest = _strip_prefixes(tag, strip)
        if add_v and not latest.startswith("v"):
            latest = f"v{latest}"
        status: Status = "unknown"
        cur_norm = current
        latest_norm = latest

        # Compare if both are simple numeric (optionally with leading v)
        cur_num = _parse_ints(_strip_prefixes(cur_norm, ("v",)))
        latest_num = _parse_ints(_strip_prefixes(latest_norm, ("v",)))
        if cur_num is not None and latest_num is not None:
            status = "ok" if cur_num == latest_num else "outdated"
        else:
            status = "ok" if cur_norm == latest_norm else "outdated"

        return Check(name, current, latest, status, f"github:{owner}/{repo}")
    except Exception as e:
        return Check(name, current, None, "unknown", f"github:{owner}/{repo}", str(e))


def _check_go_version(current: str) -> Check:
    try:
        latest = _latest_go_version()
        cur_num = _parse_ints(current)
        latest_num = _parse_ints(latest)
        status: Status
        if cur_num is not None and latest_num is not None:
            status = "ok" if cur_num == latest_num else "outdated"
        else:
            status = "ok" if current == latest else "outdated"
        return Check("GO_VERSION", current, latest, status, "go.dev")
    except Exception as e:
        return Check("GO_VERSION", current, None, "unknown", "go.dev", str(e))


def _check_node_version(current: str) -> Check:
    try:
        cur_norm = _strip_prefixes(current, ("v",))
        cur_key = _parse_ints(cur_norm)
        if cur_key is None or not cur_key:
            return Check("NODE_VERSION", current, None, "unknown", "nodejs.org", "unrecognized NODE_VERSION format")

        latest = _latest_node_patch_for_major(cur_key[0])
        if latest is None:
            return Check("NODE_VERSION", current, None, "unknown", "nodejs.org", "could not find matching major releases")

        status: Status = "ok" if cur_norm == latest else "outdated"
        return Check("NODE_VERSION", current, latest, status, "nodejs.org")
    except Exception as e:
        return Check("NODE_VERSION", current, None, "unknown", "nodejs.org", str(e))


def _check_rust_toolchain(current: str) -> Check:
    try:
        latest = _latest_rust_toolchain()
        cur_num = _parse_ints(current)
        latest_num = _parse_ints(latest)
        status: Status
        if cur_num is not None and latest_num is not None:
            status = "ok" if cur_num == latest_num else "outdated"
        else:
            status = "ok" if current == latest else "outdated"
        return Check("RUST_TOOLCHAIN", current, latest, status, "static.rust-lang.org")
    except Exception as e:
        return Check("RUST_TOOLCHAIN", current, None, "unknown", "static.rust-lang.org", str(e))


def _check_python_version(current: str) -> Check:
    try:
        # Policy: if pinned to major.minor (e.g. 3.13), treat as "tracks latest patch".
        if re.fullmatch(r"\d+\.\d+", current):
            latest_patch = _latest_python_patch_for_minor(current)
            if latest_patch is None:
                return Check("PYTHON_VERSION", current, None, "unknown", "python.org", "could not find patch releases")
            return Check(
                "PYTHON_VERSION",
                current,
                latest_patch,
                "ok",
                "python.org",
                "pinned to major.minor; uv will pick latest patch",
            )

        if re.fullmatch(r"\d+\.\d+\.\d+", current):
            major_minor = ".".join(current.split(".")[:2])
            latest_patch = _latest_python_patch_for_minor(major_minor)
            if latest_patch is None:
                return Check("PYTHON_VERSION", current, None, "unknown", "python.org", "could not find patch releases")
            status: Status = "ok" if current == latest_patch else "outdated"
            return Check("PYTHON_VERSION", current, latest_patch, status, "python.org")

        return Check("PYTHON_VERSION", current, None, "unknown", "python.org", "unrecognized PYTHON_VERSION format")
    except Exception as e:
        return Check("PYTHON_VERSION", current, None, "unknown", "python.org", str(e))


def _check_java_version(current: str) -> Check:
    try:
        latest = _latest_sdkman_java_for_identifier(current, platform="linuxx64")
        if latest is None:
            return Check("JAVA_VERSION", current, None, "unknown", "api.sdkman.io", "could not find matching Java versions")
        status: Status = "ok" if current == latest else "outdated"
        return Check("JAVA_VERSION", current, latest, status, "api.sdkman.io")
    except Exception as e:
        return Check("JAVA_VERSION", current, None, "unknown", "api.sdkman.io", str(e))


def _resolve_current(vars_map: dict[str, str], name: str) -> str | None:
    # Environment variables override bake defaults (mirrors `docker buildx bake` behavior).
    env_val = os.environ.get(name)
    if env_val is not None and env_val != "":
        return env_val
    return vars_map.get(name)


def _checks_for(bake_vars: dict[str, str]) -> list[Check]:
    def cur(name: str) -> str:
        v = _resolve_current(bake_vars, name)
        if v is None:
            raise KeyError(f"Missing variable {name} in docker-bake.hcl")
        return v

    checks: list[Check] = []

    checks.append(_check_debian_tag(cur("DEBIAN_TAG")))
    checks.append(
        _check_github_version(
            name="RUSTUP_VERSION",
            current=cur("RUSTUP_VERSION"),
            owner="rust-lang",
            repo="rustup",
            strategy="tags",
        )
    )
    checks.append(_check_rust_toolchain(cur("RUST_TOOLCHAIN")))
    checks.append(_check_go_version(cur("GO_VERSION")))
    checks.append(
        _check_github_version(
            name="BUN_VERSION",
            current=cur("BUN_VERSION"),
            owner="oven-sh",
            repo="bun",
            strip=("bun-v", "v"),
        )
    )
    checks.append(_check_node_version(cur("NODE_VERSION")))
    checks.append(_check_github_version(name="UV_VERSION", current=cur("UV_VERSION"), owner="astral-sh", repo="uv", strip=("v",)))
    checks.append(_check_python_version(cur("PYTHON_VERSION")))
    checks.append(_check_github_version(name="SDKMAN_VERSION", current=cur("SDKMAN_VERSION"), owner="sdkman", repo="sdkman-cli", strip=("v",)))
    checks.append(_check_java_version(cur("JAVA_VERSION")))
    checks.append(_check_github_version(name="DUCKDB_VERSION", current=cur("DUCKDB_VERSION"), owner="duckdb", repo="duckdb", strip=("v",)))
    checks.append(_check_github_version(name="OPENCODE_VERSION", current=cur("OPENCODE_VERSION"), owner="anomalyco", repo="opencode"))

    return checks


def _print_table(checks: Iterable[Check]) -> None:
    rows = list(checks)
    name_w = max(len("NAME"), max(len(r.name) for r in rows))
    cur_w = max(len("CURRENT"), max(len(r.current) for r in rows))
    latest_w = max(len("LATEST"), max(len(r.latest or "-") for r in rows))

    def status_str(s: Status) -> str:
        return {"ok": "OK", "outdated": "OUTDATED", "unknown": "UNKNOWN"}[s]

    print(f"{'NAME':<{name_w}}  {'CURRENT':<{cur_w}}  {'LATEST':<{latest_w}}  STATUS    SOURCE")
    print(f"{'-'*name_w}  {'-'*cur_w}  {'-'*latest_w}  --------  ------")
    for r in rows:
        latest = r.latest or "-"
        print(
            f"{r.name:<{name_w}}  {r.current:<{cur_w}}  {latest:<{latest_w}}  {status_str(r.status):<8}  {r.source}"
        )
        if r.note:
            print(f"{'':<{name_w}}  {'':<{cur_w}}  {'':<{latest_w}}           note: {r.note}")


def _print_vars(
    bake_vars: dict[str, str],
    *,
    names: list[str] | None,
    export: bool,
) -> None:
    keys = sorted(bake_vars.keys())
    if names:
        wanted = set(names)
        keys = [k for k in keys if k in wanted]
    for k in keys:
        v = _resolve_current(bake_vars, k)
        if v is None:
            continue
        prefix = "export " if export else ""
        print(f"{prefix}{k}={shlex.quote(v)}")


def _update_bake_file(
    hcl_text: str,
    checks: list[Check],
    *,
    skip_unknown: bool = True,
) -> tuple[str, list[tuple[str, str, str]]]:
    """
    Update the HCL text with latest versions from checks.

    Returns:
        (updated_hcl_text, list of (name, old_value, new_value) tuples for changes made)
    """
    changes: list[tuple[str, str, str]] = []
    updated = hcl_text

    for check in checks:
        if check.latest is None:
            continue
        if check.status == "ok":
            continue
        if check.status == "unknown" and skip_unknown:
            continue

        # Build regex to find and replace this specific variable
        pattern = re.compile(
            rf'(variable\s+"{re.escape(check.name)}"\s*\{{\s*default\s*=\s*)"([^"]*)"(\s*\}})',
            flags=re.MULTILINE,
        )

        def replacer(m: re.Match[str]) -> str:
            return f'{m.group(1)}"{check.latest}"{m.group(3)}'

        new_text, count = pattern.subn(replacer, updated)
        if count > 0 and new_text != updated:
            changes.append((check.name, check.current, check.latest))
            updated = new_text

    return updated, changes


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--file", default=BAKE_DEFAULT, help="Path to docker-bake.hcl")
    ap.add_argument("--fail", action="store_true", help="Exit non-zero if any checks are OUTDATED or UNKNOWN")
    ap.add_argument("--json", dest="json_out", action="store_true", help="Emit JSON instead of a table (check mode)")

    ap.add_argument(
        "--print-vars",
        action="store_true",
        help="Print resolved bake variables (defaults overridden by env vars) as KEY=VALUE lines",
    )
    ap.add_argument(
        "--only",
        nargs="*",
        default=None,
        help="When used with --print-vars, only print these variable names",
    )
    ap.add_argument(
        "--export",
        action="store_true",
        help="When used with --print-vars, prefix each line with `export ` (shell-friendly)",
    )

    ap.add_argument(
        "--update",
        action="store_true",
        help="Update docker-bake.hcl with latest versions (for outdated checks)",
    )
    ap.add_argument(
        "--dry-run",
        action="store_true",
        help="When used with --update, show what would be changed without writing",
    )

    args = ap.parse_args(argv)

    try:
        hcl = open(args.file, "r", encoding="utf-8").read()
    except OSError as e:
        print(f"error: could not read {args.file!r}: {e}", file=sys.stderr)
        return 2

    bake_vars = parse_bake_variables(hcl)

    if args.print_vars:
        _print_vars(bake_vars, names=args.only, export=args.export)
        return 0

    # Check mode
    try:
        checks = _checks_for(bake_vars)
    except KeyError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

    # Update mode
    if args.update:
        updated_hcl, changes = _update_bake_file(hcl, checks)

        if not changes:
            print("All versions are up-to-date. Nothing to update.")
            return 0

        # Show what will be / was changed
        print("Updates:" if not args.dry_run else "Would update:")
        name_w = max(len(name) for name, _, _ in changes)
        for name, old_val, new_val in changes:
            print(f"  {name:<{name_w}}  {old_val} -> {new_val}")

        if args.dry_run:
            print(f"\nDry run: {args.file} not modified.")
            return 0

        # Write the updated file
        try:
            with open(args.file, "w", encoding="utf-8") as f:
                f.write(updated_hcl)
            print(f"\nUpdated {args.file}")
        except OSError as e:
            print(f"error: could not write {args.file!r}: {e}", file=sys.stderr)
            return 2

        return 0

    # Normal check/report mode
    if args.json_out:
        payload = [
            {
                "name": c.name,
                "current": c.current,
                "latest": c.latest,
                "status": c.status,
                "source": c.source,
                "note": c.note,
            }
            for c in checks
        ]
        print(json.dumps(payload, indent=2))
    else:
        _print_table(checks)

    if args.fail and any(c.status != "ok" for c in checks):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

