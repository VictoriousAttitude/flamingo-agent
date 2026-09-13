#!/usr/bin/env python3
"""Check the logs of a soak run of flamingo-agent.

Reads agent.log and child.log written during a run of a known length and cadence and
checks the four things a long run must show: the cadence held, every cycle ran a child
that reported an elevated token, nothing went wrong, and resident memory did not grow.

Usage:
    soak_check.py --agent-log PATH --child-log PATH --minutes N --period-secs P
                  [--max-growth-bytes 2097152] [--min-fraction 0.9]

Exit code 0 when every check passes, 1 otherwise. Runs on any Python 3.8+.
"""

import argparse
import re
import sys
from statistics import mean

METRICS = re.compile(r" INFO metrics utc=\S+ rss_bytes=(\d+)")
CHILD_OK = re.compile(r" INFO child completed ")
ERROR = re.compile(r" ERROR ")
WARN = re.compile(r" WARN ")
CHILD_LINE = re.compile(r"^\S+ rss_bytes=\d+ elevated=(true|false)$")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--agent-log", required=True)
    parser.add_argument("--child-log", required=True)
    parser.add_argument("--minutes", type=float, required=True)
    parser.add_argument("--period-secs", type=float, required=True)
    parser.add_argument("--max-growth-bytes", type=int, default=2 * 1024 * 1024)
    parser.add_argument("--min-fraction", type=float, default=0.9)
    args = parser.parse_args()

    with open(args.agent_log, encoding="utf-8", errors="replace") as f:
        agent_lines = f.read().splitlines()
    with open(args.child_log, encoding="utf-8", errors="replace") as f:
        child_lines = [line for line in f.read().splitlines() if line.strip()]

    expected = int(args.minutes * 60 / args.period_secs)
    minimum = int(expected * args.min_fraction)
    rss = [int(m.group(1)) for line in agent_lines if (m := METRICS.search(line))]
    completed = sum(1 for line in agent_lines if CHILD_OK.search(line))
    errors = [line for line in agent_lines if ERROR.search(line)]
    warnings = [line for line in agent_lines if WARN.search(line)]
    malformed = [line for line in child_lines if not CHILD_LINE.match(line)]
    unelevated = [line for line in child_lines if line.endswith("elevated=false")]

    failures = []

    def check(ok: bool, what: str) -> None:
        print(("ok   " if ok else "FAIL ") + what)
        if not ok:
            failures.append(what)

    print(f"expected about {expected} cycles ({args.minutes:g} min at {args.period_secs:g} s)")
    check(len(rss) >= minimum, f"metrics lines: {len(rss)} (minimum {minimum})")
    check(completed >= minimum, f"child completed lines: {completed} (minimum {minimum})")
    check(
        len(child_lines) >= minimum,
        f"child.log lines: {len(child_lines)} (minimum {minimum})",
    )
    check(not malformed, f"malformed child.log lines: {len(malformed)}")
    check(not unelevated, f"child lines reporting elevated=false: {len(unelevated)}")
    check(not errors, f"ERROR lines in agent.log: {len(errors)}")
    check(not warnings, f"WARN lines in agent.log: {len(warnings)}")

    if len(rss) >= 20:
        # The first tenth is warm-up (allocator, thread pool, first child); compare the
        # steady state that follows with the last tenth of the run.
        decile = max(1, len(rss) // 10)
        early = mean(rss[decile : 2 * decile])
        late = mean(rss[-decile:])
        growth = late - early
        check(
            growth <= args.max_growth_bytes,
            f"RSS growth after warm-up: {growth / 1024:+.0f} KiB "
            f"(early mean {early / 1024:.0f} KiB, late mean {late / 1024:.0f} KiB, "
            f"max {max(rss) / 1024:.0f} KiB, limit {args.max_growth_bytes / 1024:.0f} KiB)",
        )
    else:
        check(False, f"too few RSS samples for a growth check: {len(rss)}")

    for line in (errors + warnings)[:10]:
        print("    " + line)
    if failures:
        print(f"{len(failures)} check(s) failed")
        return 1
    print("soak checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
