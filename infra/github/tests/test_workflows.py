"""The reusable self-host workflows, their plan summary, and the example
operator repository that calls them."""
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest

REPO = Path(__file__).resolve().parents[3]
WORKFLOWS = REPO / ".github/workflows"
EXAMPLE = REPO / "infra/github/example-operator-repo"
PLAN = (WORKFLOWS / "selfhost-plan.yml").read_text()
RELEASE = (WORKFLOWS / "selfhost-release.yml").read_text()
COMMAND = (WORKFLOWS / "selfhost-command.yml").read_text()
CHECKS = (WORKFLOWS / "selfhost.yml").read_text()
HOT_RELEASE = (WORKFLOWS / "release.yml").read_text()
CROSS_PLATFORM = (WORKFLOWS / "cross-platform.yml").read_text()
DURABLE = re.search(r"DURABLE: '([^']+)'", PLAN).group(1)


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


summary = load("plan_summary", REPO / "infra/github/plan_summary.py")
selfhost = load("selfhost", REPO / "infra/operator/selfhost.py")
render = load("render", REPO / "infra/runtime/render.py")


def lines(text, *prefixes):
    """The script lines of a workflow that start with one of `prefixes`."""
    return [line.strip() for line in text.splitlines() if line.strip().startswith(prefixes)]


def bash(script, **env):
    return subprocess.run(["bash", "-c", "set -euo pipefail\n" + script], env={"PATH": os.environ["PATH"], **env},
                          capture_output=True, text=True, check=True).stdout


def required_inputs(workflow):
    """The inputs a caller of a reusable workflow must set."""
    inputs = re.search(r"(?ms)^    inputs:\n(.*?)^(?:    secrets:|permissions:)", workflow).group(1)
    return [name for name, body in re.findall(r"(?m)^      (\w+):\n((?:        .*\n)+)", inputs)
            if "        required: true\n" in body]


def step(text, name):
    """A workflow step's header, up to `run`, and its script."""
    rest = text.split(f"- name: {name}\n", 1)[1]
    found = re.search(r"^( +)run: \|\n((?:\1  .*\n|\n)+)", rest, re.M)
    return rest[:found.start()], textwrap.dedent(found.group(2))


def workflow_job(text, name):
    return re.search(rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [\w-]+:\n|\Z)", text).group(1)


def job_needs(text):
    found = re.search(r"(?m)^    needs: (.+)$", text)
    return {name.strip() for name in found.group(1).strip("[]").split(",")} if found else set()


def check_inputs(text, **env):
    """Run a workflow's Check inputs step, as GitHub does: the result and what
    the step wrote to GITHUB_OUTPUT."""
    with tempfile.TemporaryDirectory() as directory:
        output = Path(directory) / "output"
        output.write_text("")
        result = subprocess.run(["bash", "-e", "-c", step(text, "Check inputs")[1]], capture_output=True, text=True,
                                env={"PATH": os.environ["PATH"], "GITHUB_OUTPUT": str(output), **env})
        return result, output.read_text()


AWS = {"CLOUD": "aws", "REGION": "us-east-1", "ROLE": "arn:aws:iam::123456789012:role/release",
       "PROJECT": "", "PROVIDER": "", "SERVICE_ACCOUNT": ""}
GCP = {"CLOUD": "gcp", "REGION": "", "ROLE": "", "PROJECT": "example-project",
       "PROVIDER": "projects/1/locations/global/workloadIdentityPools/hot-github/providers/github",
       "SERVICE_ACCOUNT": "release@example-project.iam.gserviceaccount.com"}
# The release and command workflows, with valid inputs of their own.
CALLERS = (("release", RELEASE, {"RELEASE": "3.0.1", "NAME": "hot-example", "BASE": "", "PRESET": "single-machine",
                                 "ZONE": "", "TIMEOUT": "30"}),
           ("command", COMMAND, {"NAME": "hot-example", "OPERATION": "status", "SERVICE": "", "PRESET": "single-machine",
                                 "TIMEOUT": "15"}))


def bootstrap_session_seconds():
    """The longest session the state bootstrap's apply and release roles allow."""
    role = (REPO / "infra/opentofu/bootstrap/aws/main.tf").read_text()
    allowed = re.search(r"(?m)^\s*max_session_duration\s*=\s*(.+)$", role).group(1)
    return max(int(seconds) for seconds in re.findall(r"\d+", allowed))


def resource(address, *actions, **extra):
    change = {"actions": list(actions)}
    if "importing" in extra:
        change["importing"] = extra.pop("importing")
    return {"address": address, "change": change, **extra}


class PlanSummary(unittest.TestCase):
    def summarize(self, plan):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "plan.json").write_text(json.dumps(plan))
            summary.main([str(path / "plan.json"), str(path / "summary.txt"), str(path / "durable.txt"),
                          "--durable", DURABLE])
            return (path / "summary.txt").read_text(), (path / "durable.txt").read_text()

    def test_a_plan_without_changes_writes_empty_files(self):
        # The workflow tests them with `-s`: one newline would mean a change.
        for plan in ({"resource_changes": None, "output_changes": {"o": {"actions": ["no-op"]}}},
                     {"resource_changes": [resource("aws_instance.host", "no-op")]},
                     {}):
            with self.subTest(plan=plan):
                self.assertEqual(self.summarize(plan), ("", ""))

    def test_an_update_is_a_change_but_no_deletion(self):
        self.assertEqual(self.summarize({"resource_changes": [resource("aws_instance.host", "update")]}),
                         ("update aws_instance.host\n", ""))

    def test_only_durable_deletions_are_listed_as_such(self):
        plan = {"resource_changes": [resource("aws_ebs_volume.data", "delete", "create"),
                                     resource("aws_instance.host", "delete", "create"),
                                     resource("google_sql_database_instance.hot", "delete"),
                                     resource("aws_secretsmanager_secret.generated", "update")]}
        changes, durable = self.summarize(plan)
        self.assertEqual(len(changes.splitlines()), 4)
        self.assertEqual(durable, "delete/create aws_ebs_volume.data\ndelete google_sql_database_instance.hot\n")

    def test_imports_moves_and_outputs_are_changes(self):
        # As `tofu show -json` writes them: the import and the move are no-ops.
        plan = {"resource_changes": [resource("aws_ebs_volume.data", "no-op", importing={"id": "vol-0abc"}),
                                     resource("aws_instance.host", "update", importing={"id": "i-0abc"}),
                                     resource("aws_iam_role.host", "no-op", previous_address="aws_iam_role.agent")],
                "output_changes": {"host": {"actions": ["update"]}, "same": {"actions": ["no-op"]}}}
        self.assertEqual(self.summarize(plan), (
            "import aws_ebs_volume.data\nimport/update aws_instance.host\n"
            "move aws_iam_role.host (from aws_iam_role.agent)\nupdate output.host\n", ""))


class PlanWorkflow(unittest.TestCase):
    def test_changes_come_from_the_plan_exit_code(self):
        self.assertIn("-detailed-exitcode -out=hot.tfplan || status=$?", PLAN)
        self.assertIn('plan_summary.py plan.json summary.txt durable.txt --durable "$DURABLE"', PLAN)
        self.assertIn('if [[ "$status" == 2 ]]; then echo "changes=true"', PLAN)
        self.assertNotIn('join("\\n")', PLAN)

    def test_a_plan_needs_a_remote_backend(self):
        self.assertEqual(PLAN.count('[[ -f "$SOURCE/backend_override.tf" ]] ||'), 2)
        self.assertIn("""jq -e '(.backend.type // "local") != "local"' "$STACK/.terraform/terraform.tfstate\"""", PLAN)

    @unittest.skipUnless(shutil.which("jq"), "jq is not installed")
    def test_an_s3_backend_must_lock_state(self):
        # The filter runs on the file `tofu init` writes, which holds the whole
        # backend configuration with null for what is not set.
        check = re.search(r"""jq -e '(\.backend\.type != "s3"[^']*)'""", PLAN).group(1)
        accepts = lambda type, **config: subprocess.run(  # noqa: E731
            ["jq", "-e", check], input=json.dumps({"backend": {"type": type, "config": config}}),
            capture_output=True, text=True).returncode == 0
        self.assertTrue(accepts("s3", bucket="b", use_lockfile=True, dynamodb_table=None))
        self.assertTrue(accepts("s3", bucket="b", use_lockfile=None, dynamodb_table="locks"))
        # GCS locks without being asked.
        self.assertTrue(accepts("gcs", bucket="b"))
        self.assertFalse(accepts("s3", bucket="b", use_lockfile=None, dynamodb_table=None))
        self.assertFalse(accepts("s3", bucket="b", use_lockfile=False, dynamodb_table=""))
        self.assertFalse(accepts("s3", bucket="b"))
        self.assertIn("must lock state: set use_lockfile = true", PLAN)

    def test_the_headings_name_the_installation(self):
        # Several installations can plan in one run, on the same stack.
        self.assertIn('echo "### OpenTofu plan for $CLOUD/$PRESET in $DIR"', PLAN)
        self.assertIn('echo "OpenTofu plan for \\`$CLOUD/$PRESET\\` in \\`$DIR\\` with Hot $HOT_VERSION:"', PLAN)
        # The check step, the summary, and the comment each set it themselves.
        self.assertEqual(PLAN.count("DIR: ${{ inputs.installation_dir }}"), 3)

    def test_the_saved_plan_is_named_for_its_installation(self):
        script = "\n".join(lines(PLAN, 'name="$(printf %s "$DIR"', 'echo "artifact=')) + "\n"
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            names = set()
            for installation in ("installation", "staging", "envs/prod", "envs-prod"):
                output.write_text("")
                bash(script, DIR=installation, GITHUB_OUTPUT=str(output))
                name = output.read_text().strip().removeprefix("artifact=")
                self.assertRegex(name, r"^selfhost-plan-[A-Za-z0-9._-]+$")
                names.add(name)
        self.assertEqual(len(names), 4)
        self.assertIn("name: ${{ steps.check.outputs.artifact }}", PLAN)
        self.assertIn("name: ${{ needs.plan.outputs.artifact }}", PLAN)
        self.assertNotRegex(PLAN, r"(?m)^ +name: selfhost-plan$")

    def test_the_saved_plan_outlasts_a_weekend(self):
        self.assertGreaterEqual(int(re.search(r"retention-days: (\d+)", PLAN).group(1)), 7)

    def test_a_call_that_applies_holds_its_installation(self):
        group = re.search(r"^concurrency:\n  group: (.+)$", PLAN, re.M).group(1)
        self.assertIn("inputs.apply && !inputs.drift_check && format('selfhost-{0}-{1}', github.repository, "
                      "inputs.installation_dir)", group)
        self.assertIn("-lock-timeout=30m", PLAN)

    def test_only_the_apply_job_asks_for_a_long_session(self):
        # A plan changes nothing, so the default hour is enough, and the plan
        # role runs for pull requests.
        plan, apply = PLAN.split("\n  apply:\n", 1)
        self.assertNotIn("role-duration-seconds", plan)
        self.assertIn("role-duration-seconds: ${{ inputs.aws_apply_session_seconds }}\n", apply)
        self.assertEqual(self.apply_session_default(), 14400)

    def test_the_bootstrap_role_allows_the_session_apply_asks_for(self):
        # AWS refuses a session longer than the role's maximum, so every apply
        # would fail at authentication.
        self.assertGreaterEqual(bootstrap_session_seconds(), self.apply_session_default())

    def test_a_refused_apply_session_explains_itself(self):
        # A role made by hand allows an hour unless it says otherwise. AWS's
        # refusal names neither the input nor the fix, after the reviewers
        # approve, and a re-run fails the same way.
        plan, apply = PLAN.split("\n  apply:\n", 1)
        self.assertNotIn("id: aws\n", plan)
        self.assertIn("- name: Authenticate to AWS\n        id: aws\n", apply)
        header, script = step(apply, "Explain a refused session")
        self.assertIn("if: failure() && steps.aws.outcome == 'failure'\n", header)
        self.assertIn("SESSION: ${{ inputs.aws_apply_session_seconds }}\n", header)
        # It runs before the steps a failed authentication skips.
        self.assertLess(apply.index("id: aws\n"), apply.index("- name: Explain a refused session\n"))
        self.assertLess(apply.index("- name: Explain a refused session\n"), apply.index("- name: Add the installation's configuration\n"))
        # Authentication can fail for other reasons, so it says when this applies.
        message = bash(script, SESSION="14400")
        self.assertTrue(message.startswith("::error::Nothing was applied. If AWS said DurationSeconds exceeds MaxSessionDuration,"), message)
        for expected in ("maximum session duration", "aws_apply_session_seconds (14400)", "4 hours", "state bootstrap"):
            self.assertIn(expected, message)
        self.assertEqual(bootstrap_session_seconds(), 4 * 3600)

    @staticmethod
    def apply_session_default():
        return int(re.search(r"aws_apply_session_seconds:\n\s+description: .*\n\s+type: number\n\s+default: (\d+)\n",
                             PLAN).group(1))

    @unittest.skipUnless(shutil.which("jq"), "jq is not installed")
    def test_state_the_apply_could_not_save_is_kept_only_if_encrypted(self):
        # As OpenTofu 1.13 writes it for a stack with enforced state encryption,
        # and as it would look without.
        script = step(PLAN, "Check for state the apply could not save")[1]
        kept = {'{"serial":2,"lineage":"l","meta":{},"encrypted_data":"YMJh","encryption_version":"v0"}': True,
                '{"version":4,"serial":2,"lineage":"l","outputs":{"password":{"value":"secret"}}}': False,
                "not json": False}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            for state, encrypted in kept.items():
                with self.subTest(state=state):
                    (path / "errored.tfstate").write_text(state)
                    (path / "output").write_text("")
                    bash(script, STACK=directory, GITHUB_OUTPUT=str(path / "output"))
                    self.assertEqual((path / "output").read_text(), "encrypted=true\n" if encrypted else "")

    def test_the_state_is_kept_after_a_failed_apply(self):
        apply = PLAN.split("\n  apply:\n", 1)[1]
        self.assertLess(apply.index("- name: Apply the reviewed plan"), apply.index("id: errored"))
        self.assertIn("if: always() && hashFiles(format('{0}/errored.tfstate', env.STACK)) != ''\n", apply)
        self.assertIn("if: always() && steps.errored.outputs.encrypted == 'true'\n", apply)
        self.assertLess(apply.index("id: errored"), apply.index("path: ${{ env.STACK }}/errored.tfstate\n"))
        # A re-run keeps what an earlier attempt could not save.
        self.assertIn("-errored-state-${{ github.run_attempt }}\n", apply)
        # OpenTofu's `tofu state push` cannot read an encrypted file.
        self.assertIn("copied over the state object", apply)


class ReleaseAndCommandInputs(unittest.TestCase):
    def test_each_cloud_needs_its_own_inputs(self):
        # Empty ones fail much later, with an error that names none of them.
        for name, text, inputs in CALLERS:
            for cloud, needed in ((AWS, ("REGION", "ROLE")), (GCP, ("PROJECT", "PROVIDER", "SERVICE_ACCOUNT"))):
                with self.subTest(workflow=name, cloud=cloud["CLOUD"]):
                    self.assertEqual(check_inputs(text, **inputs, **cloud)[0].returncode, 0)
                    for variable in needed:
                        result, _ = check_inputs(text, **inputs, **{**cloud, variable: ""})
                        self.assertNotEqual(result.returncode, 0, variable)
                        self.assertIn("::error::", result.stdout)
                        self.assertIn("aws_release_role" if cloud is AWS else "gcp_release_service_account", result.stdout)

    def test_the_step_is_given_those_inputs(self):
        for name, text, _ in CALLERS:
            with self.subTest(workflow=name):
                header = step(text, "Check inputs")[0]
                for variable, value in (("PRESET", "preset"), ("REGION", "aws_region"), ("ROLE", "aws_release_role"),
                                        ("PROJECT", "gcp_project"), ("PROVIDER", "gcp_workload_identity_provider"),
                                        ("SERVICE_ACCOUNT", "gcp_release_service_account")):
                    self.assertIn(f"{variable}: ${{{{ inputs.{value} }}}}\n", header)

    def test_the_preset_is_one_the_helper_knows(self):
        # The helper takes anything but scalable for a single machine, so an
        # empty variable or a typo would send a scalable fleet's command as one
        # machine's: it would believe the first host that answers.
        for name, text, inputs in CALLERS:
            for cloud in (AWS, GCP):
                for preset in ("single-machine", "scalable"):
                    with self.subTest(workflow=name, cloud=cloud["CLOUD"], preset=preset):
                        env = {**inputs, "PRESET": preset, "ZONE": "us-central1-a"}
                        result, _ = check_inputs(text, **env, **cloud)
                        self.assertEqual(result.returncode, 0, result.stdout)
                for preset in ("", "scaleable", "Scalable"):
                    with self.subTest(workflow=name, cloud=cloud["CLOUD"], preset=preset):
                        result, _ = check_inputs(text, **{**inputs, "PRESET": preset}, **cloud)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("::error::preset must be single-machine or scalable", result.stdout)
        self.assertTrue(selfhost.Installation({"name": "n", "cloud": "aws", "preset": "scalable"}, cli=None).scalable)
        self.assertFalse(selfhost.Installation({"name": "n", "cloud": "aws", "preset": "scaleable"}, cli=None).scalable)

    def test_aws_credentials_outlast_the_job(self):
        # An hour is what a role allows unless it says otherwise, so no limit
        # up to an hour asks for more.
        for name, text, inputs in CALLERS:
            for minutes, seconds in ((15, 3600), (30, 3600), (60, 3600), (61, 3660), (90, 5400), (240, 14400)):
                with self.subTest(workflow=name, minutes=minutes):
                    result, output = check_inputs(text, **{**inputs, "TIMEOUT": str(minutes)}, **AWS)
                    self.assertEqual(result.returncode, 0, result.stdout)
                    self.assertIn(f"aws_session_seconds={seconds}\n", output)
            with self.subTest(workflow=name):
                self.assertIn("role-duration-seconds: ${{ steps.check.outputs.aws_session_seconds }}\n", text)
                self.assertIn("id: check\n", step(text, "Check inputs")[0])
                # The session follows the same limit as the job and the helper.
                limit = re.search(r"timeout-minutes: (.+)\n", text).group(1)
                self.assertEqual(text.count(f"TIMEOUT: {limit}\n"), 2)

    def test_aws_refuses_a_limit_the_role_cannot_outlast(self):
        # AWS would refuse the credentials once the job had started, in an error
        # that does not name the input.
        limit = bootstrap_session_seconds() // 60
        for name, text, inputs in CALLERS:
            for minutes in (limit + 1, 241, 720):
                with self.subTest(workflow=name, minutes=minutes):
                    result, output = check_inputs(text, **{**inputs, "TIMEOUT": str(minutes)}, **AWS)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("::error::on AWS timeout_minutes can be at most 240", result.stdout)
                    self.assertEqual(output, "")
            with self.subTest(workflow=name, minutes=limit):
                result, output = check_inputs(text, **{**inputs, "TIMEOUT": str(limit)}, **AWS)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertIn(f"aws_session_seconds={bootstrap_session_seconds()}\n", output)

    def test_google_cloud_has_no_such_limit(self):
        for name, text, inputs in CALLERS:
            with self.subTest(workflow=name):
                result, _ = check_inputs(text, **{**inputs, "TIMEOUT": "241"}, **GCP)
                self.assertEqual(result.returncode, 0, result.stdout)

    def test_the_limit_is_whole_minutes(self):
        # The session and helper arithmetic has no fractions, and the AWS check
        # would otherwise report `90.5` as too long.
        for name, text, inputs in CALLERS:
            for cloud in (AWS, GCP):
                for minutes in ("90.5", "-5"):
                    with self.subTest(workflow=name, cloud=cloud["CLOUD"], minutes=minutes):
                        result, _ = check_inputs(text, **{**inputs, "TIMEOUT": minutes}, **cloud)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("::error::timeout_minutes must be a whole number", result.stdout)


class ReleaseWorkflow(unittest.TestCase):
    def test_platform_checks_run_for_releases_weekly_or_on_request(self):
        triggers = CROSS_PLATFORM.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
        self.assertEqual(set(re.findall(r"(?m)^  ([\w_]+):", triggers)),
                         {"workflow_call", "schedule", "workflow_dispatch"})
        for os_name in ("macos-latest", "windows-latest"):
            self.assertIn(f"- os: {os_name}\n", CROSS_PLATFORM)

    def test_release_uses_the_platform_workflow_at_its_own_commit(self):
        caller = workflow_job(HOT_RELEASE, "cross-platform-tests")
        self.assertIn("uses: ./.github/workflows/cross-platform.yml\n", caller)
        self.assertEqual(job_needs(caller), {"validate-release"})
        self.assertIn("needs.validate-release.outputs.is_package_only != 'true'", caller)
        gate = workflow_job(HOT_RELEASE, "release-ready")
        self.assertEqual(job_needs(gate), {"validate-release", "cross-platform-tests"})
        self.assertIn("!cancelled()", gate)
        self.assertIn("needs.validate-release.result == 'success'", gate)

    def test_every_release_publisher_waits_for_platform_tests(self):
        def prerequisites(name):
            needed = job_needs(workflow_job(HOT_RELEASE, name))
            return needed | {ancestor for parent in needed for ancestor in prerequisites(parent)}

        for publisher in ("package-cdn", "create-release", "selfhost-image", "selfhost-release-assets"):
            with self.subTest(publisher=publisher):
                self.assertIn("release-ready", prerequisites(publisher))

        # A skipped native matrix must not skip a successful package-only
        # gate, or the package CDN job downstream of it.
        cdn = workflow_job(HOT_RELEASE, "package-cdn").split("    runs-on:", 1)[0]
        self.assertIn("!cancelled()", cdn)
        for prerequisite in job_needs(cdn):
            self.assertIn(f"needs.{prerequisite}.result == 'success'", cdn)

    def test_successful_platform_tests_allow_publication(self):
        _, script = step(HOT_RELEASE, "Check release test results")
        for package_only in ("true", "false"):
            with self.subTest(package_only=package_only):
                self.assertIn("tests passed", bash(script, IS_PACKAGE_ONLY=package_only, PLATFORM_RESULT="success"))

    def test_only_package_only_releases_can_skip_platform_tests(self):
        _, script = step(HOT_RELEASE, "Check release test results")
        self.assertIn("Package-only release", bash(script, IS_PACKAGE_ONLY="true", PLATFORM_RESULT="skipped"))
        result = subprocess.run(["bash", "-e", "-c", script], capture_output=True, text=True,
                                env={"PATH": os.environ["PATH"], "IS_PACKAGE_ONLY": "false", "PLATFORM_RESULT": "skipped"})
        self.assertNotEqual(result.returncode, 0)

    def test_failed_cancelled_or_missing_platform_results_block_publication(self):
        _, script = step(HOT_RELEASE, "Check release test results")
        for package_only in ("true", "false"):
            for result in ("failure", "cancelled", "", "unexpected"):
                with self.subTest(package_only=package_only, platform_result=result):
                    checked = subprocess.run(["bash", "-e", "-c", script], capture_output=True, text=True,
                                             env={"PATH": os.environ["PATH"], "IS_PACKAGE_ONLY": package_only,
                                                  "PLATFORM_RESULT": result})
                    self.assertNotEqual(checked.returncode, 0)
                    self.assertIn("::error::macOS and Windows tests must pass", checked.stdout)

    def test_the_manifest_url(self):
        script = "\n".join(lines(RELEASE, 'default="https://', 'base="')) + '\nprintf %s "$base"\n'
        self.assertEqual(bash(script, BASE="", RELEASE="3.0.1"),
                         "https://github.com/hot-dev/hot/releases/download/v3.0.1")
        self.assertEqual(bash(script, BASE="https://mirror.example/v{release}", RELEASE="3.0.1"),
                         "https://mirror.example/v3.0.1")

    def test_it_verifies_the_manifest_as_the_host_does(self):
        self.assertNotIn("signer_identity", RELEASE)
        self.assertNotIn("--certificate-identity-regexp", RELEASE)
        self.assertIn('render.verify_manifest_signature("manifest.json", "manifest.sigstore.json")', RELEASE)
        self.assertIn('sys.path.insert(0, "hot/infra/runtime")', RELEASE)
        self.assertRegex(RELEASE, r"sparse-checkout: \|\n\s+infra/operator\n\s+infra/runtime\n")
        # The checkout comes first, so the check runs Hot's own code.
        self.assertLess(RELEASE.index("infra/runtime\n"), RELEASE.index("render.verify_manifest_signature"))

    def test_hot_attaches_only_manifests_installations_accept(self):
        job = HOT_RELEASE.split("  selfhost-release-assets:", 1)[1]
        self.assertIn('render.verify_manifest_signature("hot-selfhost-manifest.json", '
                      '"hot-selfhost-manifest.sigstore.json")', job)
        self.assertIn("if: steps.verify.outputs.attach == 'true'", job)
        self.assertLess(job.index("id: verify"), job.index("gh release upload"))

    def test_only_a_tag_installations_accept_must_verify(self):
        # A preview or prerelease only warns: its manifest was never going to verify.
        accepted = re.search(r"@(refs/tags/.+\$)$", render.RELEASE_SIGNER).group(1)
        job = HOT_RELEASE.split("  selfhost-release-assets:", 1)[1]
        self.assertIn(f'elif [[ "$GITHUB_REF" =~ ^{accepted} ]]; then', job)

    def test_hot_attaches_a_manifest_only_once_hosts_can_pull_its_image(self):
        job = HOT_RELEASE.split("  selfhost-release-assets:", 1)[1]
        header, script = step(job, "Check that hosts can pull the runtime image")
        self.assertIn("if: steps.verify.outputs.attach == 'true'\n", header)
        self.assertLess(job.index("id: verify"), job.index("- name: Check that hosts can pull the runtime image"))
        self.assertLess(job.index("- name: Check that hosts can pull the runtime image"), job.index("gh release upload"))
        # Signing in would pass for a private package, and fail every host.
        self.assertNotIn("GH_TOKEN", header)
        self.assertNotIn("TOKEN }}", script)
        self.assertNotIn("docker/login-action", job.split("- name: Upload to the GitHub release", 1)[0])

    @unittest.skipUnless(shutil.which("jq"), "jq is not installed")
    def test_a_package_hosts_cannot_read_stops_the_job(self):
        digest = "sha256:" + "a" * 64
        header, script = step(HOT_RELEASE.split("  selfhost-release-assets:", 1)[1],
                              "Check that hosts can pull the runtime image")
        # As ghcr.io answers: a private or missing package refuses the token.
        curl = textwrap.dedent("""\
            #!/bin/bash
            printf '%s\\n' "$*" >> "$CALLS"
            denied() { echo "curl: (22) The requested URL returned error: 403" >&2; exit 22; }
            case "$*" in
              *ghcr.io/token*)
                case "$ANSWER" in
                  token-denied) denied ;;
                  no-token) echo '{"errors":[{"code":"DENIED"}]}' ;;
                  *) echo '{"token":"anonymous"}' ;;
                esac ;;
              *)
                [[ "$ANSWER" != manifest-denied ]] || denied ;;
            esac
            """)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            stub = path / "curl"
            stub.write_text(curl)
            stub.chmod(0o755)
            (path / "hot-selfhost-manifest.json").write_text(
                json.dumps({"images": {"hot": f"ghcr.io/hot-dev/hot-runtime@{digest}"}}))

            def run(answer):
                (path / "calls").write_text("")
                result = subprocess.run(["bash", "-e", "-c", script], cwd=path, capture_output=True, text=True,
                                        env={"PATH": f"{path}:{os.environ['PATH']}", "CALLS": str(path / "calls"),
                                             "ANSWER": answer, "TAG_NAME": "v3.0.1"})
                return result, (path / "calls").read_text().splitlines()

            result, calls = run("readable")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("https://ghcr.io/token?service=ghcr.io&scope=repository:hot-dev/hot-runtime:pull", calls[0])
            self.assertIn(f"https://ghcr.io/v2/hot-dev/hot-runtime/manifests/{digest}", calls[1])
            self.assertIn("Authorization: Bearer anonymous", calls[1])
            # The build attaches provenance and an SBOM, so the image is an index.
            self.assertIn("application/vnd.oci.image.index.v1+json", calls[1])
            self.assertIn("-fsSI", calls[1])

            for answer in ("token-denied", "no-token", "manifest-denied"):
                with self.subTest(answer=answer):
                    result, _ = run(answer)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertTrue(result.stdout.startswith("::error::"), result.stdout)
                    self.assertIn("v3.0.1 has no self-host manifest", result.stdout)
                    self.assertIn("https://github.com/orgs/hot-dev/packages/container/hot-runtime/settings", result.stdout)
                    self.assertIn("re-run this job", result.stdout)

    def test_a_scalable_gcp_release_needs_its_zone(self):
        inputs = CALLERS[0][2]
        passes = lambda cloud, **env: check_inputs(RELEASE, **{**inputs, **env}, **cloud)[0].returncode == 0  # noqa: E731
        self.assertTrue(passes(GCP, PRESET="scalable", ZONE="us-central1-a"))
        self.assertTrue(passes(GCP, PRESET="single-machine", ZONE=""))
        self.assertTrue(passes(AWS, PRESET="scalable", ZONE=""))
        self.assertFalse(passes(GCP, PRESET="scalable", ZONE=""))
        # A region names no zone, and selfhost.py derives the region from the zone.
        self.assertFalse(passes(GCP, PRESET="scalable", ZONE="us-central1"))
        self.assertEqual(selfhost.Installation({"name": "n", "cloud": "gcp", "region": "", "zone": "us-central1-a"},
                                               cli=None).region, "us-central1")

    def test_a_scalable_release_gets_room_for_every_role(self):
        limit = "${{ inputs.timeout_minutes || (inputs.preset == 'scalable' && 60 || 30) }}"
        self.assertIn(f"timeout-minutes: {limit}\n", RELEASE)
        self.assertIn(f"TIMEOUT: {limit}\n", RELEASE)
        # The helper's own limit ends first, so it can report what it was waiting for.
        self.assertIn("--timeout $(( TIMEOUT * 54 ))", RELEASE)
        self.assertRegex(RELEASE, r"timeout_minutes:\n\s+description: .*\n\s+type: number\n\s+default: 0\n")

    def test_request_ids_are_unique_per_call(self):
        for name, text in (("release", RELEASE), ("command", COMMAND)):
            with self.subTest(workflow=name):
                self.assertNotIn("REQUEST_ID", text)
                script = "\n".join(lines(text, 'request_id="')) + '\nprintf %s "$request_id"\n'
                env = {"GITHUB_RUN_ID": "18765432101", "GITHUB_RUN_ATTEMPT": "3"}
                first, second = bash(script, **env), bash(script, **env)
                self.assertNotEqual(first, second)
                # The SSM command document's allowedPattern.
                self.assertRegex(first, r"^gha-18765432101-3-[A-Za-z0-9-]+$")
                self.assertRegex(first, r"^[A-Za-z0-9-]{1,64}$")


class CommandWorkflow(unittest.TestCase):
    def test_a_caller_must_name_its_preset(self):
        # Without its preset a command still runs on a scalable fleet, and the
        # helper believes the first host that answers. A release without it
        # fails at the pointer and a plan fails the stack's validation, so they
        # can keep their default.
        self.assertEqual(sorted(required_inputs(COMMAND)), ["cloud", "hot_version", "installation", "operation", "preset"])
        preset = re.search(r"(?m)^      preset:\n((?:        .*\n)+)", COMMAND).group(1)
        self.assertNotIn("default:", preset)

    def test_every_helper_operation_is_allowed(self):
        allowed = re.search(r'\[\[ "\$OPERATION" =~ \^\(([a-z|-]+)\)\$ \]\]', COMMAND).group(1).split("|")
        self.assertEqual(sorted(allowed), sorted(selfhost.OPERATIONS))
        example = (EXAMPLE / ".github/workflows/command.yml").read_text()
        options = re.search(r"options: \[([a-z, -]+)\]", example).group(1).split(", ")
        # The example offers them all: the README sends operators to each.
        self.assertEqual(sorted(options), sorted(allowed))

    def test_slow_operations_get_room_by_default(self):
        # The helper gives up at 90% of the limit and calls the run a failure
        # while the agent keeps working, as a dump or restore of a large local
        # database would at 15 minutes.
        limit = re.search(r"timeout-minutes: (.+)\n", COMMAND).group(1)
        slow = json.loads(re.search(r"fromJSON\('(\[[^']*\])'\)", limit).group(1))
        self.assertEqual(sorted(slow), ["backup", "deploy-now", "roll-back"])
        self.assertLessEqual(set(slow), set(selfhost.OPERATIONS))
        self.assertTrue(limit.startswith("${{ inputs.timeout_minutes || (contains("), limit)
        self.assertTrue(limit.endswith("inputs.operation) && 30 || 15) }}"), limit)
        self.assertIn("--timeout $(( TIMEOUT * 54 ))", COMMAND)
        self.assertRegex(COMMAND, r"timeout_minutes:\n\s+description: .*\n\s+type: number\n\s+default: 0\n")
        # The example takes these limits, and names them only in a comment.
        example = (EXAMPLE / ".github/workflows/command.yml").read_text()
        self.assertNotRegex(example, r"(?m)^ +timeout_minutes:")
        self.assertIn("30 minutes for deploy-now, roll-back, and backup and\n      # 15 for the rest", example)

    def test_no_command_waits_at_github(self):
        # GitHub keeps one pending job per group and cancels the one it replaces,
        # so a command held there could silently not run, or could go out after a
        # stop or a roll-back sent later and undo it. A caller's own group would do
        # the same.
        for name, workflow in (("selfhost-command.yml", COMMAND),
                               ("example command.yml", (EXAMPLE / ".github/workflows/command.yml").read_text())):
            with self.subTest(workflow=name):
                self.assertNotRegex(workflow, r"(?m)^\s*concurrency:")
        agent = (REPO / "infra/agent/hot_agent.py").read_text()
        # The agent's lock is what keeps them apart on the host: the command
        # runs, heartbeat and all, inside the same locked block.
        self.assertRegex(agent, r"with locked\(\):\n {12}agent = new_agent\(\)\n(?: {12,}.*\n)*? {12}with agent\.heartbeat\(\):\n")
        # A deploy-now that waited behind a failed attempt answers with that
        # failure, not a second attempt.
        self.assertIn("settled = self.failure_since(state, sent, request_id)", agent)
        # A wait past the helper's limit is reported, and does not run later.
        self.assertIn("deadline = expiry(expires)", agent)
        self.assertIn("--timeout $(( TIMEOUT * 54 ))", COMMAND)
        # A release still takes its turn at GitHub, in a group of its own.
        self.assertIn("group: selfhost-release-${{ github.repository }}-${{ inputs.installation }}\n", RELEASE)

    def test_the_callers_are_told_what_a_scalable_installation_refuses(self):
        # drain-and-stop would report stopped while the app and API groups replace
        # the stopped hosts, and the database is not on a scalable installation's hosts.
        single_machine = {"backup", "drain-and-stop", "roll-back"}
        self.assertLessEqual(single_machine, set(selfhost.OPERATIONS))
        operation = re.search(r"(?m)^      operation:\n        description: (.+)\n", COMMAND).group(1)
        self.assertIn("backup, drain-and-stop, and roll-back are for single-machine installations", operation)
        example = (EXAMPLE / ".github/workflows/command.yml").read_text()
        operation = re.search(r"(?m)^      operation:\n        description: (.+)\n", example).group(1)
        self.assertEqual(operation, "backup, drain-and-stop, and roll-back are for single-machine installations")


class InfrastructureChecks(unittest.TestCase):
    def test_every_action_is_pinned_to_a_commit(self):
        # dependency-audit.yml runs this on every pull request.
        result = subprocess.run(["bash", str(REPO / "scripts/check-actions-pinned.sh")], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


    def test_psql_runs_against_a_real_server(self):
        # The test skips unless both are set. psql runs in the service's image,
        # which starting the service has already pulled.
        image = re.search(r"services:\n.*?\n +image: (\S+)\n", CHECKS, re.S).group(1)
        self.assertIn(f"HOT_INFRA_TEST_POSTGRES_IMAGE: {image}\n", CHECKS)
        self.assertIn("HOT_INFRA_TEST_POSTGRES_URI: postgres://hot:hot@host.docker.internal:5432/hot\n", CHECKS)
        self.assertIn("- 5432:5432\n", CHECKS)
        self.assertEqual(image, re.search(r"(?m)^ +image: (pgvector/\S+)$", (WORKFLOWS / "hot.yml").read_text()).group(1))
        # So a change to hot.yml's image alone fails on its own pull request.
        self.assertIn("- '.github/workflows/hot.yml'\n", CHECKS)

    def test_a_built_image_proves_it_can_format_a_data_volume(self):
        # test_host.py reads the Dockerfile. This one runs in the built image,
        # and finds mkfs.ext4 on PATH the way the task worker does.
        _, script = step(CHECKS, "Smoke test the image")
        checks = re.search(r"-c \\\n\s+'([^']+)'", script).group(1).split("&&")
        self.assertIn("command -v mkfs.ext4 >/dev/null", [check.strip() for check in checks])


class ExampleRepository(unittest.TestCase):
    def test_the_release_role_is_the_release_principal(self):
        tfvars = json.loads((EXAMPLE / "installation/installation.tfvars.json").read_text())
        for caller in ("release.yml", "command.yml"):
            with self.subTest(caller=caller):
                text = (EXAMPLE / ".github/workflows" / caller).read_text()
                self.assertEqual(re.search(r"aws_release_role: (\S+)", text).group(1), tfvars["release_principal"])

    def test_the_google_cloud_lines_are_a_caller_that_works(self):
        identity = ("gcp_project", "gcp_workload_identity_provider", "gcp_release_service_account")
        for caller, workflow, needed in (("release.yml", RELEASE, identity), ("command.yml", COMMAND, identity),
                                         ("infrastructure.yml", PLAN,
                                          ("gcp_workload_identity_provider", "gcp_plan_service_account",
                                           "gcp_apply_service_account"))):
            with self.subTest(caller=caller):
                shown = re.findall(r"(?m)^ +#   (\w+): ", (EXAMPLE / ".github/workflows" / caller).read_text())
                self.assertEqual(shown, ["cloud", *needed])
                for name in shown:
                    self.assertIn(f"      {name}:\n", workflow)

    def test_the_callers_set_every_required_input(self):
        # GitHub refuses a call that leaves one out, before any job starts.
        for caller, workflow in (("release.yml", RELEASE), ("command.yml", COMMAND), ("infrastructure.yml", PLAN)):
            text = (EXAMPLE / ".github/workflows" / caller).read_text()
            needed = required_inputs(workflow)
            self.assertIn("cloud", needed)
            for name in needed:
                with self.subTest(caller=caller, input=name):
                    self.assertRegex(text, rf"(?m)^      {name}: \S")

    def test_the_callers_name_the_same_preset(self):
        # An operator moving to scalable changes it in each. The example's
        # installation names no preset, which is a single machine.
        tfvars = json.loads((EXAMPLE / "installation/installation.tfvars.json").read_text())
        self.assertEqual(tfvars["installation"].get("preset", "single-machine"), "single-machine")
        for caller in ("release.yml", "command.yml", "infrastructure.yml"):
            with self.subTest(caller=caller):
                text = (EXAMPLE / ".github/workflows" / caller).read_text()
                self.assertEqual(re.findall(r"(?m)^      preset: (\S+)$", text), ["single-machine"])
                self.assertIn("scalable", text)
        for caller in ("release.yml", "command.yml"):
            with self.subTest(caller=caller):
                self.assertIn("Scalable installations also need\n      # `preset: scalable`, above, and gcp_zone",
                              (EXAMPLE / ".github/workflows" / caller).read_text())

    def test_it_warns_of_the_apply_session_limit(self):
        text = (EXAMPLE / ".github/workflows/infrastructure.yml").read_text()
        self.assertRegex(text, r"(?m)^      aws_apply_role: \S+\n      # Apply asks for a 4-hour session")
        self.assertEqual(PlanWorkflow.apply_session_default(), 4 * 3600)
        self.assertIn("set aws_apply_session_seconds", text)

    def test_changing_the_caller_plans_and_applies_it(self):
        text = (EXAMPLE / ".github/workflows/infrastructure.yml").read_text()
        self.assertEqual(text.count("paths: ['installation/**', '.github/workflows/infrastructure.yml']"), 2)
        self.assertIn("\n  workflow_dispatch:\n", text)
        self.assertIn("apply: ${{ github.ref == 'refs/heads/main' && (github.event_name == 'push' || "
                      "github.event_name == 'workflow_dispatch') }}", text)

    def test_the_callers_pin_this_release(self):
        # Operators copy these files as they stand, and a tag without the
        # reusable workflows fails every run, so a tag's example must call that
        # tag. A version bump fails here, and on its own pull request, until
        # the pins move with it.
        pin = "v" + (REPO / "resources/version.txt").read_text().strip()
        pins = {path.name: re.findall(r"(?m)^ +(?:uses: hot-dev/hot/\.github/workflows/selfhost-\w+\.yml@|hot_version: )(\S+)$",
                                      path.read_text())
                for path in (EXAMPLE / ".github/workflows").glob("*.yml")}
        self.assertEqual(pins, {"command.yml": [pin, pin], "infrastructure.yml": [pin, pin], "release.yml": [pin]},
                         f"Bump every `uses:` and `hot_version` under {EXAMPLE.relative_to(REPO)} to {pin}.")
        self.assertIn("- 'resources/version.txt'\n", CHECKS)
        # The release gate reads only hot.yml, so this suite must run there.
        self.assertIn("python3 -m unittest discover -s infra/github/tests\n", (WORKFLOWS / "hot.yml").read_text())

    def test_the_workflow_headers_name_no_release_that_can_go_stale(self):
        for name, text in (("selfhost-plan", PLAN), ("selfhost-release", RELEASE), ("selfhost-command", COMMAND)):
            with self.subTest(workflow=name):
                self.assertIn(f"#   uses: hot-dev/hot/.github/workflows/{name}.yml@vX.Y.Z\n", text)

    def test_it_names_a_remote_backend(self):
        self.assertRegex((EXAMPLE / "installation/backend_override.tf").read_text(), r'backend "s3" \{')
        self.assertIn("use_lockfile = true", (EXAMPLE / "installation/backend_override.tf").read_text())

    def test_its_backend_is_committed(self):
        # Hot's own .gitignore leaves out backend overrides, except this one.
        ignored = subprocess.run(["git", "check-ignore", "-q", "infra/github/example-operator-repo/installation/backend_override.tf"],
                                 cwd=REPO, capture_output=True)
        if ignored.returncode == 128:
            self.skipTest("not a git checkout")
        self.assertEqual(ignored.returncode, 1, "backend_override.tf is ignored")


if __name__ == "__main__":
    unittest.main()
