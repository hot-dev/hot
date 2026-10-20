# Install PostgreSQL 18 and build pgvector for the Windows cross-platform job.
#
# The Windows build step runs this as a background job, so the Chocolatey
# install (about 9 minutes) overlaps the Rust build. It always leaves a
# status file: "ok", or why setup failed. The job's service probe decides
# whether Postgres-backed tests run.
$ErrorActionPreference = 'Stop'
$status = Join-Path $env:RUNNER_TEMP 'postgres-setup.status'
try {
  # The runner image's preinstalled PostgreSQL is older than 18, but the
  # migrations use PG18 features (uuidv7()), matching the postgres:18.x
  # service containers on Linux and postgresql@18 on macOS, so install 18
  # explicitly.
  choco install postgresql18 --yes --no-progress --params '/Password:root'
  if ($LASTEXITCODE -ne 0) { throw "choco install postgresql18 exited with $LASTEXITCODE" }
  $pgRoot = 'C:\Program Files\PostgreSQL\18'
  $pgBin = Join-Path $pgRoot 'bin'
  $env:Path = "$pgBin;$env:Path"
  $env:PGPASSWORD = 'root'
  $svc = Get-Service -Name 'postgresql*18*' -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($svc -and $svc.Status -ne 'Running') { Start-Service -Name $svc.Name }

  # Build pgvector with MSVC against this Postgres.
  git clone --depth 1 --branch v0.8.1 https://github.com/pgvector/pgvector "$env:RUNNER_TEMP\pgvector"
  if ($LASTEXITCODE -ne 0) { throw 'pgvector clone failed' }
  $vsPath = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -property installationPath
  cmd /c "`"$vsPath\VC\Auxiliary\Build\vcvars64.bat`" && set `"PGROOT=$pgRoot`" && cd /d `"$env:RUNNER_TEMP\pgvector`" && nmake /F Makefile.win && nmake /F Makefile.win install"
  if ($LASTEXITCODE -ne 0) { throw 'pgvector build failed' }

  psql -U postgres -c "CREATE ROLE hot LOGIN SUPERUSER PASSWORD 'hot';"
  if ($LASTEXITCODE -ne 0) { throw 'creating the hot role failed' }
  createdb -U postgres -O hot hot
  if ($LASTEXITCODE -ne 0) { throw 'creating the hot database failed' }
  Set-Content -Path $status -Value 'ok'
} catch {
  Set-Content -Path $status -Value "failed: $_"
}
