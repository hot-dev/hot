#!/usr/bin/env python3
"""Operate a self-hosted Hot installation through its host agent.

    selfhost.py installation.tfvars.json command OPERATION [--service S]
    selfhost.py installation.tfvars.json logs SERVICE
    selfhost.py installation.tfvars.json release VERSION

Uses the `aws` or `gcloud` CLI with the caller's identity, such as the
release role. A command goes to the agent through the SSM command document
(AWS) or the command topic (GCP); the agent's report comes back through the
installation's log, matched by request ID. Nothing here reaches the host over
the network, and nothing needs SSH. The release workflow and the command
workflow run this same script.

Prints the agent's report as JSON (or as Markdown with --summary FILE) and
exits 1 when the agent rejects the command or the deploy fails.
"""
import argparse
import datetime
import json
import secrets
import subprocess
import sys
import time

OPERATIONS = ("deploy-now", "status", "diagnostics", "logs", "backup", "restart-service", "drain-and-stop")
# Final states per operation; anything else is progress or another request.
FAILED = {"failed", "rejected"}
STATUS_STREAM = "hot-agent-status"


class CliError(Exception):
    pass


class Cli:
    """Runs a cloud CLI and returns its parsed JSON output."""

    def __call__(self, *args, stdin=None):
        result = subprocess.run(list(args), input=stdin, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            lines = (result.stderr or result.stdout).strip().splitlines()
            raise CliError((lines[-1] if lines else f"{args[0]} exited {result.returncode}")[:300])
        return json.loads(result.stdout or "null")


class Installation:
    def __init__(self, document, cli, clock=time.time, sleep=time.sleep):
        config = document.get("installation", document)
        self.name = config["name"]
        self.cloud = config["cloud"]
        self.region = config.get("region", "")
        self.project = config.get("project", "")
        self.cli = cli
        self.clock = clock
        self.sleep = sleep

    # -- sending ---------------------------------------------------------------

    def send(self, operation, service, request_id):
        if self.cloud == "aws":
            parameters = f"operation={operation},service={service or 'none'},requestId={request_id}"
            self.cli("aws", "ssm", "send-command", "--region", self.region, "--output", "json",
                     "--document-name", f"hot-{self.name}-command",
                     "--targets", f"Key=tag:HotInstallation,Values={self.name}",
                     "--parameters", parameters, "--comment", f"Hot {operation}")
        else:
            message = json.dumps({"operation": operation, "service": service, "request_id": request_id})
            self.cli("gcloud", "pubsub", "topics", "publish", f"hot-{self.name}-commands",
                     "--project", self.project, "--message", message, "--format", "json")

    def point_at(self, release):
        if self.cloud == "aws":
            self.cli("aws", "ssm", "put-parameter", "--region", self.region, "--output", "json",
                     "--name", f"/hot/{self.name}/release", "--value", release, "--type", "String", "--overwrite")
        else:
            self.cli("gcloud", "secrets", "versions", "add", f"hot-{self.name}-release", "--project", self.project,
                     "--data-file=-", "--format", "json", stdin=json.dumps({"release": release}))

    # -- reports -----------------------------------------------------------------

    def reports(self, request_id, since):
        """The agent's reports for one request, oldest first."""
        if self.cloud == "aws":
            result = self.cli("aws", "logs", "filter-log-events", "--region", self.region, "--output", "json",
                              "--log-group-name", f"/hot/{self.name}", "--log-stream-names", STATUS_STREAM,
                              "--start-time", str(int(since * 1000)),
                              "--filter-pattern", f'{{ $.request_id = "{request_id}" }}')
            return [json.loads(event["message"]) for event in (result or {}).get("events", [])]
        started = datetime.datetime.fromtimestamp(since, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        query = (f'logName="projects/{self.project}/logs/hot-{self.name}-status" '
                 f'AND jsonPayload.request_id="{request_id}" AND timestamp>="{started}"')
        entries = self.cli("gcloud", "logging", "read", query, "--project", self.project,
                           "--format", "json", "--order", "asc", "--limit", "20") or []
        return [entry["jsonPayload"] for entry in entries]

    def run(self, operation, service="", request_id="", timeout=600, poll=10):
        """Send one command and wait for the agent's answer to it."""
        if operation not in OPERATIONS:
            raise CliError(f"Unknown operation {operation}")
        request_id = request_id or f"cli-{secrets.token_hex(6)}"
        # Log timestamps come from the host, so allow for some clock skew.
        since = self.clock() - 120
        self.send(operation, service, request_id)
        deadline = self.clock() + timeout
        while True:
            reports = self.reports(request_id, since)
            if reports:
                return reports[-1]
            if self.clock() >= deadline:
                where = ("the command's invocation in Systems Manager" if self.cloud == "aws"
                         else "the agent's journal through the serial console")
                raise CliError(f"The agent did not answer {request_id} within {timeout} seconds; check {where}")
            self.sleep(poll)

    def release(self, version, request_id="", timeout=1800):
        """Point the installation at a release and deploy it now."""
        self.point_at(version)
        report = self.run("deploy-now", request_id=request_id, timeout=timeout)
        if report.get("state") in ("deployed", "current") and report.get("release") != version:
            report = {**report, "state": "failed",
                      "error": f"The agent reports release {report.get('release')}, not {version}"}
        return report


def markdown(report):
    """A job summary for a report. Logs go in a code block; everything else
    is shown as JSON."""
    lines = [f"### {report.get('installation', 'Hot')}: {report.get('state', '?')}", ""]
    if report.get("error"):
        lines += [f"**Error:** {report['error']}", ""]
    logs = report.get("logs")
    body = {key: value for key, value in report.items() if key != "logs"}
    lines += ["```json", json.dumps(body, indent=2), "```"]
    if logs is not None:
        fence = "````" if any("```" in line for line in logs) else "```"
        lines += ["", f"Last {len(logs)} lines of `{report.get('service')}`:", "", fence, *logs, fence]
    return "\n".join(lines) + "\n"


def main(argv=None, cli=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tfvars", help="the installation's tfvars JSON")
    parser.add_argument("--request-id", default="")
    parser.add_argument("--timeout", type=int, default=0, help="seconds to wait for the agent")
    parser.add_argument("--summary", help="also append a Markdown report to this file")
    sub = parser.add_subparsers(dest="action", required=True)
    command = sub.add_parser("command")
    command.add_argument("operation", choices=OPERATIONS)
    command.add_argument("--service", default="")
    logs = sub.add_parser("logs")
    logs.add_argument("service")
    release = sub.add_parser("release")
    release.add_argument("version")
    args = parser.parse_args(argv)
    with open(args.tfvars) as handle:
        installation = Installation(json.load(handle), cli or Cli())
    try:
        if args.action == "release":
            report = installation.release(args.version, args.request_id, args.timeout or 1800)
        elif args.action == "logs":
            report = installation.run("logs", args.service, args.request_id, args.timeout or 300)
        else:
            report = installation.run(args.operation, args.service, args.request_id, args.timeout or 600)
    except CliError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, indent=2))
    if args.summary:
        with open(args.summary, "a") as handle:
            handle.write(markdown(report))
    return 1 if report.get("state") in FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
