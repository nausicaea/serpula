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
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
from collections.abc import Generator, Iterable
from dataclasses import dataclass, field

RDN: str = "net.nausicaea.serpula"


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
        self.log_dir = self.cache_dir / "logs"
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
        a = [self.subcommand()]
        if len(self._tags) > 0:
            a.append(f"-t={','.join(self._tags)}")
        if self._exclude_caches:
            a.append("--exclude-caches")
        for exclude in self._excludes:
            a.append(f"-e={exclude}")
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
    with path.open(mode="wt") as f:
        f.write(serialize_env_content(data))
    path.chmod(0o600)
    path.parent.chmod(0o700)


def ensure_secrets_scaffold(context: Context) -> None:
    if context.secrets_file.exists():
        return
    variables = [
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ]
    secrets = {v: "" for v in variables}
    save_env_file(secrets.items(), context.secrets_file)


def build_restic_env(context: Context) -> dict[str, str]:
    allowed = {
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_DEFAULT_REGION",
        "AWS_SESSION_TOKEN",
    }
    secrets = dict(
        filter(lambda e: e[0] in allowed, load_env_file(context.secrets_file))
    )
    required = ["RESTIC_REPOSITORY", "RESTIC_PASSWORD"]
    for r in required:
        if r not in secrets:
            raise ValueError(
                f"Environment variable {r} must be set in {context.secrets_file}"
            )
    secrets["RESTIC_CACHE_DIR"] = str(context.cache_dir)
    return secrets


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

    secrets = dict(load_env_file(context.secrets_file))
    topic = secrets.get(ntfy_topic)
    if topic is not None:
        topic = topic.strip()
        if len(topic) > 0:
            return topic

    topic = generate_topic(context.ntfy_prefix)
    secrets[ntfy_topic] = topic
    save_env_file(secrets.items(), context.secrets_file)
    return topic


def notify_failure(
    context: Context,
    priority: Priority,
    subcommand: str,
    message: str,
) -> None:
    title = f"serpula: restic {subcommand} failed on {context.host_name}"
    topic = get_ntfy_topic(context)
    client = http.client.HTTPSConnection(
        context.ntfy_server_fqdn,
        context=ssl.create_default_context(),
    )
    client.request(
        http.HTTPMethod.POST,
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
    if not status.is_success:
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

    content: bytes = ET.tostring(plist, encoding="utf-8", xml_declaration=False)
    header = b"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
"""
    return header + content


def cmd_proxy(context: Context, args: list[str]) -> None:
    restic = shutil.which("restic")
    if restic is None:
        raise ValueError("Cannot find restic in the PATH")
    lock_path = context.lock_file
    try:
        with lock_path.open(mode="wb") as f:
            fcntl.flock(f, fcntl.LOCK_EX)

            subprocess.run(
                [restic, *args],
                check=True,
                env=build_restic_env(context),
            )
    except Exception as e:
        notify_failure(context, Priority.DEFAULT, args[0], str(e))
        raise


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
        nargs="*",
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
        nargs="*",
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
    destination: Path = namespace.destination.expanduser().resolve(strict=True)
    sources: list[Path] = [
        s.expanduser().resolve(strict=True) for s in namespace.source
    ]
    context = Context.load()
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
        self.assertTrue(self.ctx.ntfy_prefix is None)

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
        self.assertEqual(self.ctx.log_dir, Path(f"/home/Library/Caches/{RDN}/logs"))

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
                "-t=a,b,c",
                "-e=A",
                "-e=B",
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
        self.assertTrue(parse_var_assignment("   A ") is None)


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
        self.assertTrue("A" in m)
        self.assertEqual(m["A"], "B")


def test() -> None:
    """
    Run unit and documentation tests.
    """
    print("Running unit tests")
    unittest.main(module=__name__, exit=False)


def main() -> None:
    args = sys.argv

    if len(args) < 2:
        raise ValueError("Too few arguments supplied.")

    context = Context.load()

    subcommand = args[1].lower()
    match subcommand:
        case "install":
            cmd_install(context, args[1:])
        case _:
            cmd_proxy(context, args[1:])


if __name__ == "__main__":
    main()
