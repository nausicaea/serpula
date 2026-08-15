#!/usr/bin/env python3

"""
Wraps calls to restic for backup operations. The goal of this script is to send
notifications using an "ntfy.sh"-compatible service in case of failures during
backup operations.

The script is designed for macOS. The `install` subcommand will install launchd
plist jobs for backups, backup pruning, and restore testing. The schedules are
currently hard-coded on install, but you can later edit the plist files to your
needs. Secrets are stored in `~/Library/Application Support/net.nausicaea.serpula/secrets/env`.
"""

from __future__ import annotations

import abc
import argparse
import enum
import fcntl
import http
import http.client
import os
import secrets
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
from collections.abc import Generator, Iterable
from dataclasses import dataclass, field
from unittest import mock

RDN: str = "net.nausicaea.serpula"
RESTIC: str | None = shutil.which("restic")


@dataclass
class Context:
    home: Path
    host_name: str
    script: Path
    runtime_dir: Path = field(init=False)
    data_dir: Path = field(init=False)
    cache_dir: Path = field(init=False)
    log_dir: Path = field(init=False)
    lock_file: Path = field(init=False)
    secrets_file: Path = field(init=False)
    ntfy_server_fqdn: str = field(default="ntfy.sh")
    ntfy_prefix: str | None = field(default=None)

    @classmethod
    def load(cls) -> "Context":
        return cls(
            home=Path.home(),
            host_name=socket.gethostname(),
            script=Path(__file__).resolve(strict=True),
        )

    def __post_init__(self) -> None:
        self.runtime_dir = self.home / "Library" / "Application Support" / RDN
        self.data_dir = self.runtime_dir
        self.cache_dir = self.home / "Library" / "Caches" / RDN
        self.log_dir = self.home / "Library" / "Logs" / RDN
        self.lock_file = self.runtime_dir / "serpula.lock"
        self.secrets_file = self.data_dir / "secrets" / "env"


class Schedule(abc.ABC):
    @abc.abstractmethod
    def build_xml(self, parent: ET.Element) -> None: ...


class Interval(Schedule):
    def __init__(self, seconds: int) -> None:
        self.seconds = seconds

    def build_xml(self, parent: ET.Element) -> None:
        key = ET.SubElement(parent, "key")
        key.text = "StartInterval"
        value = ET.SubElement(parent, "integer")
        value.text = str(self.seconds)


class Calendar(Schedule):
    def __init__(self, weekday: int | None, hour: int, minute: int) -> None:
        self.weekday = weekday
        self.hour = hour
        self.minute = minute

    def build_xml(self, parent: ET.Element) -> None:
        key = ET.SubElement(parent, "key")
        key.text = "StartCalendarInterval"
        value = ET.SubElement(parent, "dict")
        if self.weekday is not None:
            weekday_key = ET.SubElement(value, "key")
            weekday_key.text = "Weekday"
            weekday_value = ET.SubElement(value, "integer")
            weekday_value.text = str(self.weekday)
        hour_key = ET.SubElement(value, "key")
        hour_key.text = "Hour"
        hour_value = ET.SubElement(value, "integer")
        hour_value.text = str(self.hour)
        minute_key = ET.SubElement(value, "key")
        minute_key.text = "Minute"
        minute_value = ET.SubElement(value, "integer")
        minute_value.text = str(self.minute)


class Job(abc.ABC):
    @abc.abstractmethod
    def subcommand(self) -> str: ...

    @abc.abstractmethod
    def schedule(self) -> Schedule: ...

    @abc.abstractmethod
    def args(self) -> list[str]: ...


class Backup(Job):
    def __init__(
        self,
        schedule: Schedule,
        tags: list[str],
        exclude_caches: bool,
        excludes: list[str],
        sources: list[Path],
    ) -> None:
        self._subcommand = "backup"
        self._schedule = schedule
        self._tags = tags
        self._exclude_caches = exclude_caches
        self._excludes = excludes
        self._sources = sources

    def subcommand(self) -> str:
        return self._subcommand

    def schedule(self) -> Schedule:
        return self._schedule

    def args(self) -> list[str]:
        a = [self.subcommand(), "--json"]
        if len(self._tags) > 0:
            a.append(f"--tag={','.join(self._tags)}")
        if self._exclude_caches:
            a.append("--exclude-caches")
        for exclude in self._excludes:
            a.append(f"--exclude={exclude}")
        for source in self._sources:
            a.append(str(source))
        return a


class Forget(Job):
    def __init__(
        self,
        schedule: Schedule,
        keep_hourly: int,
        keep_daily: int,
        keep_weekly: int,
        keep_monthly: int,
        keep_yearly: int,
    ) -> None:
        self._subcommand = "forget"
        self._schedule = schedule
        self._keep_hourly = keep_hourly
        self._keep_daily = keep_daily
        self._keep_weekly = keep_weekly
        self._keep_monthly = keep_monthly
        self._keep_yearly = keep_yearly

    def subcommand(self) -> str:
        return self._subcommand

    def schedule(self) -> Schedule:
        return self._schedule

    def args(self) -> list[str]:
        return [
            self.subcommand(),
            "--json",
            "--prune",
            f"--keep-hourly={self._keep_hourly}",
            f"--keep-daily={self._keep_daily}",
            f"--keep-weekly={self._keep_weekly}",
            f"--keep-monthly={self._keep_monthly}",
            f"--keep-yearly={self._keep_yearly}",
        ]


class Check(Job):
    def __init__(self, schedule: Schedule, read_data_subset: str) -> None:
        self._subcommand = "check"
        self._schedule = schedule
        self._read_data_subset = read_data_subset

    def subcommand(self) -> str:
        return self._subcommand

    def schedule(self) -> Schedule:
        return self._schedule

    def args(self) -> list[str]:
        return [
            self.subcommand(),
            "--json",
            f"--read-data-subset={self._read_data_subset}",
        ]


class Priority(enum.Enum):
    # Really long vibration bursts, default notification sound with a pop-over notification.
    MAX = 5
    # Long vibration burst, default notification sound with a pop-over notification.
    HIGH = 4
    # Short default vibration and sound. Default notification behavior.
    DEFAULT = 3
    # No vibration or sound. Notification will not visibly show up until notification drawer is pulled down.
    LOW = 2
    # No vibration or sound. The notification will be under the fold in "Other notifications".
    MIN = 1


def parse_var_assignment(line: str) -> tuple[str, str] | None:
    parts = line.split(sep="=", maxsplit=1)
    if len(parts) != 2:
        return None
    return (parts[0].strip(), parts[1].strip())


def parse_env_content(lines: Iterable[str]) -> Generator[tuple[str, str], None, None]:
    for i, line in enumerate(lines):
        stripped_line = line.strip()
        if len(stripped_line) == 0 or stripped_line.startswith("#"):
            continue
        ass = parse_var_assignment(stripped_line)
        if ass is None:
            raise ValueError(f"Invalid variable assignment on line {i + 1}")
        yield ass


def serialize_env_content(data: Iterable[tuple[str, str]]) -> str:
    return "\n".join(f"{k}={v}" for k, v in data)


def load_env_file(path: Path) -> Generator[tuple[str, str], None, None]:
    with path.open(mode="rt") as f:
        yield from parse_env_content(f)


def save_env_file(data: Iterable[tuple[str, str]], path: Path) -> None:
    # BUGFIX: `path.parent` (e.g. ".../secrets/") is never created anywhere
    # else in this script, so on a first-ever run this used to blow up with
    # FileNotFoundError. Make this function self-sufficient instead of
    # relying on callers to have created the directory already.
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open(mode="wt") as f:
        f.write(serialize_env_content(data))
    path.chmod(0o600)
    path.parent.chmod(0o700)


def ensure_directories(context: Context) -> None:
    """
    Create the directories restic/launchd need that aren't already handled by
    save_env_file(): the runtime dir (holds the lock file), the cache dir
    (RESTIC_CACHE_DIR), and the log dir (launchd StandardOut/ErrorPath).
    Safe to call repeatedly.
    """
    for directory in (context.runtime_dir, context.cache_dir, context.log_dir):
        directory.mkdir(parents=True, exist_ok=True)


def ensure_secrets_scaffold(context: Context) -> None:
    if context.secrets_file.exists():
        return
    variables = [
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ]
    env_vars = {v: "" for v in variables}
    save_env_file(env_vars.items(), context.secrets_file)


def build_restic_env(context: Context) -> dict[str, str]:
    allowed = {
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_DEFAULT_REGION",
        "AWS_SESSION_TOKEN",
    }
    env_vars = dict(
        filter(lambda e: e[0] in allowed, load_env_file(context.secrets_file))
    )
    required = ["RESTIC_REPOSITORY", "RESTIC_PASSWORD"]
    for r in required:
        # BUGFIX: previously only checked key *presence*. A freshly scaffolded
        # secrets file has these keys present but empty, which used to pass
        # this check and only fail later, opaquely, inside restic itself.
        if r not in env_vars or len(env_vars[r]) == 0:
            raise ValueError(
                f"Environment variable {r} must be set in {context.secrets_file}"
            )
    # BUGFIX: this used to return *only* the filtered secrets, replacing the
    # subprocess environment entirely and dropping PATH, HOME, TMPDIR, LANG,
    # etc. That can break restic backends (rclone/sftp/ssh helpers, temp
    # file handling, ...) that rely on ambient environment variables.
    env = dict(os.environ)
    env.update(env_vars)
    env["RESTIC_CACHE_DIR"] = str(context.cache_dir)
    return env


def generate_topic(prefix: str | None) -> str:
    hexes = secrets.token_hex(24)
    if prefix is not None:
        return f"{prefix}-{hexes}"
    else:
        return hexes


def get_ntfy_topic(context: Context) -> str:
    ntfy_topic = "NTFY_TOPIC"
    topic = os.environ.get(ntfy_topic)
    if topic is not None:
        topic = topic.strip()
        if len(topic) > 0:
            return topic

    # BUGFIX: don't assume the secrets file already exists. notify()
    # calls this too, and it must not itself crash (masking the original
    # failure) just because `install`/ensure_secrets_scaffold() never ran.
    if context.secrets_file.exists():
        env_vars = dict(load_env_file(context.secrets_file))
    else:
        env_vars = {}
    topic = env_vars.get(ntfy_topic)
    if topic is not None:
        topic = topic.strip()
        if len(topic) > 0:
            return topic

    topic = generate_topic(context.ntfy_prefix)
    env_vars[ntfy_topic] = topic
    save_env_file(env_vars.items(), context.secrets_file)
    return topic


def notify(
    context: Context,
    priority: Priority,
    title: str,
    message: str,
) -> None:
    topic = get_ntfy_topic(context)
    client = http.client.HTTPSConnection(
        context.ntfy_server_fqdn,
        context=ssl.create_default_context(),
    )
    client.request(
        "POST",
        f"/{topic}",
        headers={
            "Host": context.ntfy_server_fqdn,
            "Content-Type": "text/plain; charset=utf-8",
            "X-Title": title,
            "X-Priority": priority.value,
        },
        body=message.encode("utf-8"),
    )
    response = client.getresponse()
    status = http.HTTPStatus(response.status)
    if not (200 <= status < 300):
        data = response.read()
        raise http.client.HTTPException(
            f"HTTP {status} {status.phrase} ({status.description}): {data!r}"
        )


def plist_label(context: Context, subcommand: str) -> str:
    max_name_len = 255
    rdn = f"{RDN}.{subcommand}"
    return rdn[:max_name_len]


def plist_document(context: Context, job: Job) -> bytes:
    plist = ET.Element("plist", attrib={"version": "1.0"})
    plist_dict = ET.SubElement(plist, "dict")
    label_key = ET.SubElement(plist_dict, "key")
    label_key.text = "Label"
    label_value = ET.SubElement(plist_dict, "string")
    label_value.text = plist_label(context, job.subcommand())
    program_arguments_key = ET.SubElement(plist_dict, "key")
    program_arguments_key.text = "ProgramArguments"
    program_arguments_value = ET.SubElement(plist_dict, "array")
    argument_0 = ET.SubElement(program_arguments_value, "string")
    argument_0.text = str(context.script)
    for arg in job.args():
        argument_i = ET.SubElement(program_arguments_value, "string")
        argument_i.text = arg
    job.schedule().build_xml(plist_dict)
    run_at_load_key = ET.SubElement(plist_dict, "key")
    run_at_load_key.text = "RunAtLoad"
    ET.SubElement(plist_dict, "false")
    standard_out_path_key = ET.SubElement(plist_dict, "key")
    standard_out_path_key.text = "StandardOutPath"
    standard_out_path_value = ET.SubElement(plist_dict, "string")
    standard_out_path_value.text = str(
        context.log_dir / f"{job.subcommand()}.stdout.log"
    )
    standard_err_path_key = ET.SubElement(plist_dict, "key")
    standard_err_path_key.text = "StandardErrorPath"
    standard_err_path_value = ET.SubElement(plist_dict, "string")
    standard_err_path_value.text = str(
        context.log_dir / f"{job.subcommand()}.stderr.log"
    )
    process_type_key = ET.SubElement(plist_dict, "key")
    process_type_key.text = "ProcessType"
    process_type_value = ET.SubElement(plist_dict, "string")
    process_type_value.text = "Background"
    low_prio_io_key = ET.SubElement(plist_dict, "key")
    low_prio_io_key.text = "LowPriorityIO"
    ET.SubElement(plist_dict, "true")
    environment_variables_key = ET.SubElement(plist_dict, "key")
    environment_variables_key.text = "EnvironmentVariables"
    environment_variables_value = ET.SubElement(plist_dict, "dict")
    path_key = ET.SubElement(environment_variables_value, "key")
    path_key.text = "PATH"
    path_value = ET.SubElement(environment_variables_value, "string")
    path_value.text = str(Path(RESTIC).parent) if RESTIC is not None else ""

    content: bytes = ET.tostring(plist, encoding="utf-8", xml_declaration=False)
    header = b"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
"""
    return header + content


def cmd_proxy(context: Context, args: list[str]) -> None:
    if RESTIC is None:
        raise ValueError("Cannot find restic in the PATH")
    # BUGFIX: nothing used to create runtime_dir/cache_dir/log_dir, so the
    # very first scheduled run (lock file, restic cache) would fail.
    ensure_directories(context)
    lock_path = context.lock_file
    try:
        with lock_path.open(mode="wb") as f:
            fcntl.flock(f, fcntl.LOCK_EX)

            subprocess.run(
                [RESTIC, *args],
                check=True,
                env=build_restic_env(context),
            )
    except Exception as e:
        title = f"serpula: restic {args[0]} failed on {context.host_name}"
        notify(context, Priority.DEFAULT, title, str(type(e)))
        raise
    title = f"serpula: restic {args[0]} succeeded on {context.host_name}"
    notify(context, Priority.LOW, title, "")


def cmd_install(context: Context, args: list[str]) -> None:
    parser = argparse.ArgumentParser(
        prog="serpula install",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
        description="Install launchd plist files for automated, scheduled backups",
    )
    parser.add_argument(
        "-D",
        "--destination",
        type=Path,
        default=Path(f"{context.home}/Library/LaunchAgents"),
        help="Define where the launchd plist files will be written to",
    )
    parser.add_argument(
        "-t",
        "--tag",
        action="append",
        default=[context.host_name],
        help="Add tags for the new snapshot.",
    )
    parser.add_argument(
        "--no-exclude-caches",
        action="store_true",
        help="Don't exclude cache directories that are marked with a CACHEDIR.TAG file.",
    )
    parser.add_argument(
        "-e",
        "--exclude",
        action="append",
        default=[],
        help="Exclude a pattern.",
    )
    parser.add_argument(
        "-H",
        "--keep-hourly",
        type=int,
        default=24,
        help="Keep the last n hourly snapshots.",
    )
    parser.add_argument(
        "-d",
        "--keep-daily",
        type=int,
        default=14,
        help="Keep the last n daily snapshots.",
    )
    parser.add_argument(
        "-w",
        "--keep-weekly",
        type=int,
        default=4,
        help="Keep the last n weekly snapshots.",
    )
    parser.add_argument(
        "-m",
        "--keep-monthly",
        type=int,
        default=12,
        help="Keep the last n monthly snapshots.",
    )
    parser.add_argument(
        "-y",
        "--keep-yearly",
        type=int,
        default=10,
        help="Keep the last n yearly snapshots.",
    )
    parser.add_argument(
        "--read-data-subset",
        default="10%",
        help="Read a subset of data packs.",
    )
    parser.add_argument(
        "source",
        nargs="+",
        type=Path,
        help="The files and folders to include in the backup.",
    )
    namespace = parser.parse_args(args[1:])
    if RESTIC is None:
        raise ValueError("Cannot find restic in the PATH")
    # BUGFIX: ~/Library/LaunchAgents isn't guaranteed to exist on a fresh
    # macOS user account (it's only created the first time something else
    # installs an agent), so resolve(strict=True) used to blow up here too.
    destination: Path = namespace.destination.expanduser()
    destination.mkdir(parents=True, exist_ok=True)
    destination = destination.resolve(strict=True)
    sources: list[Path] = [
        s.expanduser().resolve(strict=True) for s in namespace.source
    ]
    # BUGFIX: this used to silently discard the `context` argument and
    # reload a fresh one, which happened to be equivalent but was confusing
    # and meant this function ignored context injected by its caller/tests.
    ensure_directories(context)
    ensure_secrets_scaffold(context)
    get_ntfy_topic(context)

    jobs = [
        Backup(
            Interval(3600),
            namespace.tag,
            not namespace.no_exclude_caches,
            namespace.exclude,
            sources,
        ),
        Forget(
            Calendar(0, 3, 30),
            namespace.keep_hourly,
            namespace.keep_daily,
            namespace.keep_weekly,
            namespace.keep_monthly,
            namespace.keep_yearly,
        ),
        Check(Calendar(None, 2, 0), namespace.read_data_subset),
    ]

    for job in jobs:
        label = plist_label(context, job.subcommand())
        path = destination / f"{label}.plist"
        xml_data = plist_document(context, job)
        with path.open(mode="wb") as f:
            f.write(xml_data)


class TestGlobals(unittest.TestCase):
    def test_rdn(self) -> None:
        self.assertEqual(RDN, "net.nausicaea.serpula")


class TestContext(unittest.TestCase):
    def setUp(self) -> None:
        self.ctx = Context(
            home=Path("/home"),
            host_name="localhost",
            script=Path("/x/bin/script"),
        )

    def test_default_ntfy_server_fqdn(self) -> None:
        self.assertEqual(self.ctx.ntfy_server_fqdn, "ntfy.sh")

    def test_default_ntfy_prefix(self) -> None:
        self.assertIsNone(self.ctx.ntfy_prefix)

    def test_data_dir(self) -> None:
        self.assertEqual(
            self.ctx.data_dir,
            Path(f"/home/Library/Application Support/{RDN}"),
        )

    def test_runtime_dir(self) -> None:
        self.assertEqual(
            self.ctx.runtime_dir,
            Path(f"/home/Library/Application Support/{RDN}"),
        )

    def test_cache_dir(self) -> None:
        self.assertEqual(self.ctx.cache_dir, Path(f"/home/Library/Caches/{RDN}"))

    def test_log_dir(self) -> None:
        self.assertEqual(self.ctx.log_dir, Path(f"/home/Library/Logs/{RDN}"))

    def test_lock_file(self) -> None:
        self.assertEqual(
            self.ctx.lock_file,
            Path(f"/home/Library/Application Support/{RDN}/serpula.lock"),
        )

    def test_secrets_file(self) -> None:
        self.assertEqual(
            self.ctx.secrets_file,
            Path(f"/home/Library/Application Support/{RDN}/secrets/env"),
        )


class TestContextSpecial(unittest.TestCase):
    def test_load_side_effects(self) -> None:
        ctx = Context.load()
        self.assertEqual(ctx.home, Path.home())
        self.assertEqual(ctx.host_name, socket.gethostname())
        self.assertEqual(ctx.script, Path(__file__).resolve(strict=True))


class TestSchedule(unittest.TestCase):
    def test_interval_build_xml(self) -> None:
        e = ET.Element("test")
        Interval(60).build_xml(e)

        children = list(e)
        self.assertEqual(len(children), 2)

        key, value = children
        self.assertEqual(key.tag, "key")
        self.assertEqual(key.text, "StartInterval")
        self.assertEqual(value.tag, "integer")
        self.assertEqual(value.text, "60")

    def test_calendar_build_xml(self) -> None:
        e = ET.Element("test")
        Calendar(1, 2, 3).build_xml(e)

        key, value = list(e)
        self.assertEqual(key.text, "StartCalendarInterval")
        self.assertEqual(value.tag, "dict")

        # value should contain Weekday, Hour, Minute key/value pairs
        grandchildren = list(value)
        texts = [c.text for c in grandchildren]
        self.assertEqual(
            texts,
            ["Weekday", "1", "Hour", "2", "Minute", "3"],
        )

        e = ET.Element("test")
        Calendar(None, 2, 3).build_xml(e)

        key, value = list(e)
        self.assertEqual(key.text, "StartCalendarInterval")
        self.assertEqual(value.tag, "dict")

        # value should contain Weekday, Hour, Minute key/value pairs
        grandchildren = list(value)
        texts = [c.text for c in grandchildren]
        self.assertEqual(
            texts,
            ["Hour", "2", "Minute", "3"],
        )


class TestJob(unittest.TestCase):
    def test_backup_subcommand(self) -> None:
        j = Backup(Interval(60), [], False, [], [])
        self.assertEqual(j.subcommand(), "backup")

    def test_backup_schedule(self) -> None:
        s = Interval(60)
        j = Backup(s, [], False, [], [])
        self.assertEqual(j.schedule(), s)

    def test_backup_args(self) -> None:
        j = Backup(
            Interval(60), ["a,b", "c"], False, ["A", "B"], [Path("X"), Path("Y")]
        )
        self.assertEqual(
            j.args(),
            [
                "backup",
                "--json",
                "--tag=a,b,c",
                "--exclude=A",
                "--exclude=B",
                "X",
                "Y",
            ],
        )

    def test_forget_subcommand(self) -> None:
        j = Forget(Interval(60), 1, 2, 3, 4, 5)
        self.assertEqual(j.subcommand(), "forget")

    def test_forget_schedule(self) -> None:
        s = Interval(60)
        j = Forget(s, 1, 2, 3, 4, 5)
        self.assertEqual(j.schedule(), s)

    def test_forget_args(self) -> None:
        j = Forget(Interval(60), 1, 2, 3, 4, 5)
        self.assertEqual(
            j.args(),
            [
                "forget",
                "--json",
                "--prune",
                "--keep-hourly=1",
                "--keep-daily=2",
                "--keep-weekly=3",
                "--keep-monthly=4",
                "--keep-yearly=5",
            ],
        )

    def test_check_subcommand(self) -> None:
        j = Check(Interval(60), "30%")
        self.assertEqual(j.subcommand(), "check")

    def test_check_schedule(self) -> None:
        s = Interval(60)
        j = Check(s, "30%")
        self.assertEqual(j.schedule(), s)

    def test_check_args(self) -> None:
        j = Check(Interval(60), "30%")
        self.assertEqual(
            j.args(),
            [
                "check",
                "--json",
                "--read-data-subset=30%",
            ],
        )


class TestPriority(unittest.TestCase):
    def test_values(self) -> None:
        self.assertEqual(Priority.MAX.value, 5)
        self.assertEqual(Priority.HIGH.value, 4)
        self.assertEqual(Priority.DEFAULT.value, 3)
        self.assertEqual(Priority.LOW.value, 2)
        self.assertEqual(Priority.MIN.value, 1)


class TestParseVarAssignment(unittest.TestCase):
    def test_happy_path(self) -> None:
        valid = [
            ("A=", ("A", "")),
            (" A  =   ", ("A", "")),
            ("A=B", ("A", "B")),
            (" A = B ", ("A", "B")),
            (" A = B = C", ("A", "B = C")),
        ]
        for orig, expected in valid:
            self.assertEqual(parse_var_assignment(orig), expected)

    def test_expected_failures(self) -> None:
        self.assertIsNone(parse_var_assignment("   A "))


class TestParseEnvContent(unittest.TestCase):
    def test_happy_path(self) -> None:
        m = dict(
            parse_env_content(
                [
                    "# This is a comment",
                    "   # This too is a comment",
                    "A = B",
                ]
            )
        )

        self.assertEqual(len(m), 1)
        self.assertIn("A", m)
        self.assertEqual(m["A"], "B")

    def test_blank_lines_are_skipped(self) -> None:
        m = dict(parse_env_content(["", "   ", "A=B", "\t"]))
        self.assertEqual(m, {"A": "B"})

    def test_invalid_line_raises_with_1_indexed_line_number(self) -> None:
        with self.assertRaisesRegex(ValueError, "line 2"):
            list(parse_env_content(["A=B", "not-an-assignment"]))


class TestSerializeEnvContent(unittest.TestCase):
    def test_basic(self) -> None:
        self.assertEqual(
            serialize_env_content([("A", "1"), ("B", "two words")]),
            "A=1\nB=two words",
        )

    def test_empty(self) -> None:
        self.assertEqual(serialize_env_content([]), "")

    def test_round_trips_with_parse_env_content(self) -> None:
        data = [("A", "1"), ("B", "2")]
        text = serialize_env_content(data)
        self.assertEqual(list(parse_env_content(text.splitlines())), data)


class TestSaveLoadEnvFile(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        # Nested, not-yet-existing directory: exercises the mkdir fix.
        self.path = Path(self.tmp.name) / "nested" / "secrets" / "env"

    def test_round_trip(self) -> None:
        data = [("A", "1"), ("B", "two words"), ("C", "")]
        save_env_file(data, self.path)
        self.assertEqual(list(load_env_file(self.path)), data)

    def test_creates_missing_parent_directories(self) -> None:
        self.assertFalse(self.path.parent.exists())
        save_env_file([("A", "1")], self.path)
        self.assertTrue(self.path.exists())

    def test_sets_restrictive_permissions(self) -> None:
        save_env_file([("A", "1")], self.path)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.path.parent.stat().st_mode & 0o777, 0o700)

    def test_overwrites_existing_file(self) -> None:
        save_env_file([("A", "1")], self.path)
        save_env_file([("A", "2"), ("B", "3")], self.path)
        self.assertEqual(list(load_env_file(self.path)), [("A", "2"), ("B", "3")])


class TestEnsureDirectories(unittest.TestCase):
    def test_creates_expected_directories(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = Context(home=Path(tmp), host_name="h", script=Path("/x/s"))
            self.assertFalse(ctx.runtime_dir.exists())
            ensure_directories(ctx)
            self.assertTrue(ctx.runtime_dir.is_dir())
            self.assertTrue(ctx.cache_dir.is_dir())
            self.assertTrue(ctx.log_dir.is_dir())

    def test_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            ctx = Context(home=Path(tmp), host_name="h", script=Path("/x/s"))
            ensure_directories(ctx)
            ensure_directories(ctx)
            self.assertTrue(ctx.log_dir.is_dir())


class TestEnsureSecretsScaffold(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.ctx = Context(home=Path(self.tmp.name), host_name="h", script=Path("/x/s"))

    def test_creates_scaffold_with_expected_keys(self) -> None:  # trufflehog:ignore
        ensure_secrets_scaffold(self.ctx)
        data = dict(load_env_file(self.ctx.secrets_file))
        self.assertEqual(
            set(data.keys()),
            {
                "RESTIC_REPOSITORY",
                "RESTIC_PASSWORD",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
            },
        )
        self.assertTrue(all(v == "" for v in data.values()))

    def test_does_not_overwrite_existing_file(self) -> None:
        save_env_file([("RESTIC_REPOSITORY", "s3:bucket")], self.ctx.secrets_file)
        ensure_secrets_scaffold(self.ctx)
        data = dict(load_env_file(self.ctx.secrets_file))
        self.assertEqual(data, {"RESTIC_REPOSITORY": "s3:bucket"})


class TestBuildResticEnv(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.ctx = Context(home=Path(self.tmp.name), host_name="h", script=Path("/x/s"))

    def test_missing_required_var_raises(self) -> None:
        save_env_file([("RESTIC_REPOSITORY", "s3:bucket")], self.ctx.secrets_file)
        with self.assertRaises(ValueError):
            build_restic_env(self.ctx)

    def test_empty_required_var_raises(self) -> None:
        save_env_file(
            [("RESTIC_REPOSITORY", ""), ("RESTIC_PASSWORD", "hunter2")],
            self.ctx.secrets_file,
        )
        with self.assertRaises(ValueError):
            build_restic_env(self.ctx)

    def test_filters_disallowed_vars_and_sets_cache_dir(self) -> None:
        save_env_file(
            [
                ("RESTIC_REPOSITORY", "s3:bucket"),
                ("RESTIC_PASSWORD", "hunter2"),
                ("SOME_UNRELATED_VAR", "nope"),
            ],
            self.ctx.secrets_file,
        )
        env = build_restic_env(self.ctx)
        self.assertNotIn("SOME_UNRELATED_VAR", env)
        self.assertEqual(env["RESTIC_CACHE_DIR"], str(self.ctx.cache_dir))

    def test_preserves_ambient_environment(self) -> None:
        save_env_file(
            [("RESTIC_REPOSITORY", "s3:bucket"), ("RESTIC_PASSWORD", "hunter2")],
            self.ctx.secrets_file,
        )
        with mock.patch.dict(os.environ, {"PATH": "/custom/bin"}):
            env = build_restic_env(self.ctx)
        self.assertEqual(env["PATH"], "/custom/bin")

    def test_secrets_override_ambient_environment(self) -> None:
        save_env_file(
            [
                ("RESTIC_REPOSITORY", "s3:bucket"),
                ("RESTIC_PASSWORD", "hunter2"),
                ("AWS_ACCESS_KEY_ID", "from-file"),
            ],
            self.ctx.secrets_file,
        )
        with mock.patch.dict(os.environ, {"AWS_ACCESS_KEY_ID": "ambient"}):
            env = build_restic_env(self.ctx)
        self.assertEqual(env["AWS_ACCESS_KEY_ID"], "from-file")


class TestGenerateTopic(unittest.TestCase):
    def test_without_prefix_is_48_hex_chars(self) -> None:
        topic = generate_topic(None)
        self.assertEqual(len(topic), 48)
        bytes.fromhex(topic)

    def test_with_prefix(self) -> None:
        topic = generate_topic("myprefix")
        self.assertTrue(topic.startswith("myprefix-"))
        suffix = topic[len("myprefix-") :]
        self.assertEqual(len(suffix), 48)
        bytes.fromhex(suffix)


class TestGetNtfyTopic(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.ctx = Context(home=Path(self.tmp.name), host_name="h", script=Path("/x/s"))

    def test_environment_variable_takes_precedence(self) -> None:
        save_env_file([("NTFY_TOPIC", "from-file")], self.ctx.secrets_file)
        with mock.patch.dict(os.environ, {"NTFY_TOPIC": "from-env"}):
            self.assertEqual(get_ntfy_topic(self.ctx), "from-env")

    def test_falls_back_to_secrets_file(self) -> None:
        save_env_file([("NTFY_TOPIC", "from-file")], self.ctx.secrets_file)
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("NTFY_TOPIC", None)
            self.assertEqual(get_ntfy_topic(self.ctx), "from-file")

    def test_generates_and_persists_when_absent(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("NTFY_TOPIC", None)
            topic = get_ntfy_topic(self.ctx)
        self.assertEqual(len(topic), 48)
        persisted = dict(load_env_file(self.ctx.secrets_file))
        self.assertEqual(persisted["NTFY_TOPIC"], topic)

    def test_works_when_secrets_file_does_not_exist_yet(self) -> None:
        self.assertFalse(self.ctx.secrets_file.exists())
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("NTFY_TOPIC", None)
            topic = get_ntfy_topic(self.ctx)
        self.assertEqual(len(topic), 48)
        self.assertTrue(self.ctx.secrets_file.exists())

    def test_preserves_existing_secrets_when_generating(self) -> None:
        save_env_file([("RESTIC_REPOSITORY", "s3:bucket")], self.ctx.secrets_file)
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("NTFY_TOPIC", None)
            get_ntfy_topic(self.ctx)
        persisted = dict(load_env_file(self.ctx.secrets_file))
        self.assertEqual(persisted["RESTIC_REPOSITORY"], "s3:bucket")
        self.assertIn("NTFY_TOPIC", persisted)


class TestNotifyFailure(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.ctx = Context(
            home=Path(self.tmp.name), host_name="myhost", script=Path("/x/s")
        )

    def test_success_path_posts_expected_request(self) -> None:  # trufflehog:ignore
        mock_response = mock.Mock()
        mock_response.status = 200
        mock_conn = mock.Mock()
        mock_conn.getresponse.return_value = mock_response

        with (
            mock.patch.dict(os.environ, {"NTFY_TOPIC": "my-topic"}),
            mock.patch(
                "http.client.HTTPSConnection", return_value=mock_conn
            ) as mock_https,
        ):
            notify(self.ctx, Priority.HIGH, "backup", "boom")

        mock_https.assert_called_once()
        args, kwargs = mock_conn.request.call_args
        self.assertEqual(args[0], "POST")
        self.assertEqual(args[1], "/my-topic")
        self.assertEqual(kwargs["headers"]["X-Priority"], Priority.HIGH.value)
        self.assertEqual(kwargs["body"], b"boom")

    def test_failure_status_raises(self) -> None:
        mock_response = mock.Mock()
        mock_response.status = 500
        mock_response.read.return_value = b"server error"
        mock_conn = mock.Mock()
        mock_conn.getresponse.return_value = mock_response

        with (
            mock.patch.dict(os.environ, {"NTFY_TOPIC": "my-topic"}),
            mock.patch("http.client.HTTPSConnection", return_value=mock_conn),
        ):
            with self.assertRaises(http.client.HTTPException):
                notify(self.ctx, Priority.DEFAULT, "backup", "boom")


class TestPlistLabel(unittest.TestCase):
    def setUp(self) -> None:
        self.ctx = Context(home=Path("/home"), host_name="h", script=Path("/x/s"))

    def test_normal_subcommand(self) -> None:
        self.assertEqual(
            plist_label(self.ctx, "backup"), "net.nausicaea.serpula.backup"
        )

    def test_truncates_to_255_chars(self) -> None:
        long_subcommand = "x" * 300
        label = plist_label(self.ctx, long_subcommand)
        self.assertEqual(len(label), 255)
        self.assertTrue(label.startswith(f"{RDN}."))


class TestPlistDocument(unittest.TestCase):
    def _plist_dict_pairs(self, document: bytes) -> dict[str, ET.Element]:
        root = ET.fromstring(document)
        d = root.find("dict")
        self.assertIsNotNone(d)
        children = list(d)  # type: ignore
        return dict(
            zip((c.text for c in children[0::2] if c is not None), children[1::2])  # type: ignore
        )

    def setUp(self) -> None:
        self.ctx = Context(
            home=Path("/home/tester"),
            host_name="myhost",
            script=Path("/usr/local/bin/serpula.py"),
        )

    def test_header_and_root(self) -> None:
        job = Check(Calendar(None, 2, 0), "5%")
        doc = plist_document(self.ctx, job)
        self.assertTrue(doc.startswith(b'<?xml version="1.0" encoding="UTF-8"?>'))
        self.assertIn(b"<!DOCTYPE plist PUBLIC", doc)
        root = ET.fromstring(doc)
        self.assertEqual(root.tag, "plist")
        self.assertEqual(root.get("version"), "1.0")

    def test_backup_job_program_arguments_and_paths(self) -> None:
        job = Backup(Interval(3600), ["tag1", "tag2"], True, ["*.tmp"], [Path("/data")])
        doc = plist_document(self.ctx, job)
        pairs = self._plist_dict_pairs(doc)

        self.assertEqual(pairs["Label"].text, "net.nausicaea.serpula.backup")

        program_args = [s.text for s in pairs["ProgramArguments"]]
        self.assertEqual(
            program_args,
            [
                str(self.ctx.script),
                "backup",
                "--json",
                "--tag=tag1,tag2",
                "--exclude-caches",
                "--exclude=*.tmp",
                "/data",
            ],
        )

        self.assertEqual(pairs["StartInterval"].text, "3600")
        self.assertEqual(
            pairs["StandardOutPath"].text, str(self.ctx.log_dir / "backup.stdout.log")
        )
        self.assertEqual(
            pairs["StandardErrorPath"].text, str(self.ctx.log_dir / "backup.stderr.log")
        )
        self.assertEqual(pairs["ProcessType"].text, "Background")
        self.assertEqual(pairs["RunAtLoad"].tag, "false")
        self.assertEqual(pairs["LowPriorityIO"].tag, "true")

    def test_check_job_uses_calendar_schedule(self) -> None:
        job = Check(Calendar(None, 2, 30), "10%")
        doc = plist_document(self.ctx, job)
        pairs = self._plist_dict_pairs(doc)
        schedule_dict = pairs["StartCalendarInterval"]
        self.assertEqual(schedule_dict.tag, "dict")
        texts = [c.text for c in schedule_dict]
        self.assertEqual(texts, ["Hour", "2", "Minute", "30"])


def test() -> None:
    """
    Run unit tests.

    You are expected to invoke the tests with

    ```
    python3 -c 'import serpula; serpula.test()'
    ```
    """
    unittest.main(module=__name__, exit=False)


def main() -> None:
    args = sys.argv

    if len(args) < 2:
        raise ValueError("Too few arguments supplied.")

    context = Context.load()

    subcommand = args[1].lower()
    if subcommand == "install":
        cmd_install(context, args[1:])
    else:
        cmd_proxy(context, args[1:])


if __name__ == "__main__":
    main()
