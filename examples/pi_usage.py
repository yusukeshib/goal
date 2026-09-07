#!/usr/bin/env python3
"""Optional, stdlib-only pi JSONL usage reporter and byte-preserving filter.

Import PiUsageReporter, feed original JSONL records to consume(), then call
finish(complete=True) only when the caller knows the upstream run succeeded.
The CLI never infers success from EOF. Nested tool usage must be disjoint from
assistant usage, and must be in the final toolResult message.usage field.
Details and content are ignored, including mirrored usage. Coverage counts
usage records, not model calls: a tool usage record may aggregate many calls.
The default path is GOAL_USAGE_PATH, or usage.json beside GOAL_RESULT_PATH
for older controllers. With neither configured reporting is disabled. This
only covers new wrapper invocations; there is no retroactive backfill.
Costs are source estimates, not invoices. Unknown costs stay unknown; coverage
metrics describe partial reported totals. Stable message IDs/timestamps and
final toolCallIds suppress duplicate events; identity-less finals cannot be
safely deduplicated. No event content is retained or written to diagnostics.
"""

import hashlib
import json
import math
import os
import sys
import tempfile

MAX_LINE_BYTES = 16 * 1024 * 1024
TOKEN_FIELDS = {
    "input": "input_tokens",
    "output": "output_tokens",
    "cacheRead": "cache_read_tokens",
    "cacheWrite": "cache_write_tokens",
    "totalTokens": "total_tokens",
}


def _number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    try:
        return value if math.isfinite(value) and value >= 0 else None
    except (OverflowError, ValueError):
        return None


class PiUsageReporter:
    """Write cumulative snapshots after final usage events; all errors nonfatal."""

    def __init__(self, path=None):
        if path is None:
            path = os.environ.get("GOAL_USAGE_PATH")
            if not path and os.environ.get("GOAL_RESULT_PATH"):
                path = os.path.join(os.path.dirname(os.environ["GOAL_RESULT_PATH"]), "usage.json")
        self.path = path
        self._seen = set()
        self._metrics = {}
        self._cost = None
        self._warned = False

    def _warn(self):
        if not self._warned:
            self._warned = True
            try:
                sys.stderr.write("pi_usage: some usage could not be processed or reported\n")
            except Exception:
                pass

    def _add(self, name, value):
        total = self._metrics.get(name, 0) + value
        if _number(total) is None:
            raise ValueError("metric overflow")
        self._metrics[name] = total

    def _usage(self, usage):
        self._add("usage_records", 1)
        if not isinstance(usage, dict):
            usage = {}
        for field, name in TOKEN_FIELDS.items():
            if field in usage:
                value = _number(usage[field])
                if value is None:
                    self._warn()
                else:
                    self._add(name, value)
        cost = usage.get("cost")
        # Pi reports a total alongside component costs. Never sum both.
        if isinstance(cost, dict):
            cost = cost.get("total")
        cost = _number(cost)
        if cost is None:
            self._add("cost_missing_records", 1)
        else:
            total = (self._cost or 0) + cost
            if _number(total) is None:
                raise ValueError("cost overflow")
            self._cost = total
            self._add("cost_reported_records", 1)

    def consume(self, line):
        if not self.path:
            return
        try:
            size = len(line) if isinstance(line, bytes) else len(line.encode("utf-8"))
            if size > MAX_LINE_BYTES:
                raise ValueError("oversized record")
            event = json.loads(line)
            if not isinstance(event, dict) or event.get("type") != "message_end":
                return
            message = event.get("message")
            if not isinstance(message, dict):
                return
            role = message.get("role")
            if role == "assistant":
                identity = message.get("id", message.get("messageId"))
                if identity is None and message.get("timestamp") is not None:
                    canonical = json.dumps(message, sort_keys=True, separators=(",", ":"))
                    identity = [message["timestamp"], hashlib.sha256(canonical.encode("utf-8")).hexdigest()]
                usages = [message.get("usage")]
            elif role == "toolResult":
                identity = message.get("toolCallId")
                if "usage" not in message:
                    return  # Ordinary tools do not imply missing model costs.
                usages = [message["usage"]]
            else:
                return
            key = None if identity is None else (role, json.dumps(identity, sort_keys=True))
            if key is not None and key in self._seen:
                return
            # A malformed/overflowing event must not leave half-applied totals.
            old_metrics, old_cost = self._metrics.copy(), self._cost
            try:
                for usage in usages:
                    self._usage(usage)
            except Exception:
                self._metrics, self._cost = old_metrics, old_cost
                raise
            if key is not None:
                self._seen.add(key)
            self.finish()
        except Exception:
            self._warn()

    def finish(self, complete=False):
        """Persist latest totals. Completion is explicit, not inferred from EOF.

        Even complete snapshots can have unknown costs: inspect coverage metrics.
        Missing cost fields never become zero, while an explicit zero is retained.
        """
        if not self.path:
            return
        temp = None
        try:
            snapshot = {
                "schema_version": 1,
                "costs": [] if self._cost is None else [{"currency": "USD", "amount": self._cost}],
                "metrics": [
                    {"name": name, "unit": "tokens" if name.endswith("_tokens") else "count", "value": value}
                    for name, value in sorted(self._metrics.items())
                ],
                "complete": bool(complete),
            }
            data = json.dumps(snapshot, allow_nan=False).encode("utf-8") + b"\n"
            target = os.path.abspath(self.path)
            fd, temp = tempfile.mkstemp(prefix=".pi-usage-", dir=os.path.dirname(target))
            with os.fdopen(fd, "wb") as output:
                os.fchmod(output.fileno(), 0o600)
                output.write(data)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temp, target)
            temp = None
        except Exception:
            self._warn()
        finally:
            if temp is not None:
                try:
                    os.unlink(temp)
                except OSError:
                    pass


def main():
    reporter = PiUsageReporter()
    source, destination = sys.stdin.buffer, sys.stdout.buffer
    oversized = False
    while True:
        line = source.readline(MAX_LINE_BYTES + 1)
        if not line:
            break
        if oversized or len(line) > MAX_LINE_BYTES:
            if reporter.path:
                reporter._warn()
            oversized = not line.endswith(b"\n")
        else:
            reporter.consume(line)
        destination.write(line)
        destination.flush()
    reporter.finish()  # EOF is not evidence of upstream success.


if __name__ == "__main__":
    main()
