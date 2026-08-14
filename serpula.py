#!/usr/bin/env python3

import abc
import argparse
import dataclasses
import enum
import fcntl
import http
import http.client
import os
import secrets
import socket
import ssl
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path
from collections.abc import Generator, Iterable

RESTIC = "restic"


@dataclasses.dataclass(frozen=True)
class Context:
    rdn: str
    runtime_dir: Path
    data_dir: Path
    cache_dir: Path
    log_dir: Path
    ntfy_server_fqdn: str
    ntfy_prefix: str | None
    host_name: str
    script: Path

    @classmethod
    def load(cls) -> "Context":
        rdn = "net.nausicaea.serpula"
        home = Path.home()
        data_dir = home / "Library" / "Application Support" / rdn
        cache_dir = home / "Library" / "Caches" / rdn
        return cls(
            rdn=rdn,
            data_dir=data_dir,
            runtime_dir=data_dir,
            cache_dir=cache_dir,
            log_dir=cache_dir / "logs",
            ntfy_server_fqdn="ntfy.sh",
            ntfy_prefix=None,
            host_name=socket.gethostname(),
            script=Path(__file__).resolve(strict=True),
        )

    @property
    def lock_file(self) -> Path:
        return self.runtime_dir / "serpula.lock"

    @property
    def secrets_file(self) -> Path:
        return self.data_dir / "secrets" / "env"


class Schedule(abc.ABC):
    @abc.abstractmethod
    def build_xml(self, parent: ET.Element): ...


class Interval(Schedule):
    def __init__(self, seconds: int):
        self.seconds = seconds

    def build_xml(self, parent: ET.Element):
        key = ET.SubElement(parent, "key")
        key.text = "StartInterval"
        value = ET.SubElement(parent, "integer")
        value.text = str(self.seconds)


class Calendar(Schedule):
    def __init__(self, weekday: int | None, hour: int, minute: int):
        self.weekday = weekday
        self.hour = hour
        self.minute = minute

    def build_xml(self, parent: ET.Element):
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
    ):
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
    ):
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
    def __init__(self, schedule: Schedule, read_data_subset: str):
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


def save_env_file(data: Iterable[tuple[str, str]], path: Path):
    with path.open(mode="wt") as f:
        f.write(serialize_env_content(data))


def ensure_secrets_scaffold(context: Context):
    if context.secrets_file.exists():
        return
    variables = [
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ]
    secrets = ((v, "") for v in variables)
    save_env_file(secrets, context.secrets_file)


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
    NTFY_TOPIC = "NTFY_TOPIC"
    topic = os.environ.get(NTFY_TOPIC)
    if topic is not None:
        topic = topic.strip()
        if len(topic) > 0:
            return topic

    secrets = dict(load_env_file(context.secrets_file))
    topic = secrets.get(NTFY_TOPIC)
    if topic is not None:
        topic = topic.strip()
        if len(topic) > 0:
            return topic

    topic = generate_topic(context.ntfy_prefix)
    secrets[NTFY_TOPIC] = topic
    save_env_file(secrets.items(), context.secrets_file)
    return topic


def notify_failure(
    context: Context,
    priority: Priority,
    subcommand: str,
    message: str,
):
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
            "X-Priority": priority.name,
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
    MAX_NAME_LEN = 255
    rdn = f"{context.rdn}.{subcommand}"
    return rdn[:MAX_NAME_LEN]


def plist_document(context: Context, job: Job) -> str:
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

    content = ET.tostring(plist, encoding="utf-8", xml_declaration=False)
    header = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
"""
    return header.encode("utf-8") + content


def cmd_proxy(context: Context, args: list[str]):
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
        notify_failure(context, Priority.DEFAULT, args[0], str(e))
        raise


def cmd_install(context: Context, args: list[str]):
    parser = argparse.ArgumentParser(
        prog="serpula install",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
        description="Install launchd plist files for automated, scheduled backups",
    )
    parser.add_argument(
        "-D",
        "--destination",
        type=Path,
        default=Path("~/Library/LaunchAgents"),
        help="Define where the launchd plist files will be written to",
    )
    parser.add_argument(
        "-t",
        "--tag",
        nargs="*",
        default=[],
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


def main():
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
