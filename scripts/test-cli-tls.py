#!/usr/bin/env python3
"""Test the real CLI with disposable PostgreSQL servers and generated certificates."""
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import time
import uuid


def run(*args, ok=True, env=None):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=120, env=env)
    print("+", " ".join(map(str, args)), flush=True)
    print(result.stdout, end="", flush=True)
    if ok and result.returncode:
        raise RuntimeError(f"command exited {result.returncode}")
    return result


default_binary = Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "debug/kryzhen"
binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else default_binary).resolve())
containers = []
password = "  p:'\"\\ with spaces  "  # Public test fixture, never a real credential.
try:
    with tempfile.TemporaryDirectory(prefix="kryzhen-cli-tls-") as directory:
        root = Path(directory)
        migrations = root / "migrations"
        migrations.mkdir()
        run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=Kryzhen test CA", "-keyout", str(root / "ca.key"), "-out", str(root / "ca.crt"))
        for name, subject in [("server", "localhost"), ("client", "postgres")]:
            run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={subject}",
                "-keyout", str(root / f"{name}.key"), "-out", str(root / f"{name}.csr"))
            extension = root / f"{name}.ext"
            extension.write_text("basicConstraints=CA:FALSE\nextendedKeyUsage=" +
                                 ("serverAuth\nsubjectAltName=DNS:localhost\n" if name == "server" else "clientAuth\n"))
            run("openssl", "x509", "-req", "-days", "1", "-in", str(root / f"{name}.csr"),
                "-CA", str(root / "ca.crt"), "-CAkey", str(root / "ca.key"), "-CAcreateserial",
                "-extfile", str(extension), "-out", str(root / f"{name}.crt"))
        (root / "pg_hba.conf").write_text("local all all trust\nhostssl all all all cert clientcert=verify-full\n")

        def server(tls):
            name = "kryzhen-test-" + uuid.uuid4().hex[:12]
            containers.append(name)
            args = ["docker", "run", "--detach", "--rm", "--name", name,
                    "-e", f"POSTGRES_PASSWORD={password}", "-p", "127.0.0.1::5432"]
            if tls:
                args += ["--mount", f"type=bind,src={root},dst=/certs,readonly", "postgres:17", "sh", "-c",
                         "install -o postgres -g postgres -m 600 /certs/server.key /var/lib/postgresql/server.key && "
                         "install -o postgres -g postgres -m 644 /certs/server.crt /certs/ca.crt /certs/pg_hba.conf /var/lib/postgresql/ && "
                         "exec docker-entrypoint.sh postgres -c ssl=on "
                         "-c ssl_cert_file=/var/lib/postgresql/server.crt -c ssl_key_file=/var/lib/postgresql/server.key "
                         "-c ssl_ca_file=/var/lib/postgresql/ca.crt -c hba_file=/var/lib/postgresql/pg_hba.conf"]
            else:
                args += ["postgres:17"]
            run(*args)
            port = run("docker", "inspect", "--format",
                       '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}', name).stdout.strip()
            for _ in range(60):
                if run("docker", "exec", name, "pg_isready", "-h", "127.0.0.1", "-U", "postgres", ok=False).returncode == 0:
                    return port
                time.sleep(0.5)
            run("docker", "logs", name, ok=False)
            raise RuntimeError("test database did not become ready")

        tls_port = server(True)
        plain_port = server(False)
        identity = ["--ssl-client-cert", str(root / "client.crt"), "--ssl-client-key", str(root / "client.key")]
        ca = ["--ssl-root-cert", str(root / "ca.crt")]

        def check(label, port, mode, extra=(), host="localhost", success=True,
                  password_file=None, env_password=None, explicit_password=None):
            if explicit_password is not None:
                credentials = ["--password", explicit_password]
            elif password_file is None and env_password is None:
                credentials = ["--password", password]
            else:
                credentials = []
            env = dict(os.environ, PGPASSFILE=str(password_file or root / "absent-passwords"))
            env.pop("PGPASSWORD", None)
            if env_password is not None:
                env["PGPASSWORD"] = env_password
            result = run(binary, "--database", "postgres", "--user", "postgres", *credentials,
                         "--host", host, "--port", port, "--root", str(migrations), "--dry-run",
                         "--sslmode", mode, *extra, ok=False, env=env)
            if (result.returncode == 0) != success:
                raise AssertionError(f"{label}: unexpected exit {result.returncode}")
            print("PASS:", label, flush=True)

        check("verify-full trusts CA and presents client identity", tls_port, "verify-full", ca + identity)
        check("verify-full rejects hostname mismatch", tls_port, "verify-full", ca + identity, host="127.0.0.1", success=False)
        check("verify-ca permits hostname mismatch with trusted chain", tls_port, "verify-ca", ca + identity, host="127.0.0.1")
        check("verify-full rejects untrusted CA", tls_port, "verify-full", identity, success=False)
        check("verify-ca rejects untrusted CA", tls_port, "verify-ca", identity, success=False)
        check("require encrypts without verifying server certificate", tls_port, "require", identity, host="127.0.0.1")
        check("server rejects absent client identity", tls_port, "verify-full", ca, success=False)
        for mode in ["require", "verify-ca", "verify-full"]:
            check(f"{mode} rejects plaintext-only server", plain_port, mode, success=False)
        check("prefer permits plaintext-only server", plain_port, "prefer")
        check("disable connects to plaintext-only server", plain_port, "disable")
        pgpass = root / "passwords"
        pgpass.touch(mode=0o600)
        escaped = password.replace("\\", "\\\\").replace(":", "\\:")
        entry = f"localhost:{plain_port}:postgres:postgres:{escaped}"
        for label, content, success in [
            ("quotes, spaces, colon and backslash survive pgpass authentication", entry, True),
            ("CRLF, comments and incomplete entries", f"# comment\r\ninvalid\r\n{entry}\r\n", True),
            ("first matching entry wins", entry + "\n*:*:*:*:wrong", True),
            ("first matching wrong password is not skipped", "*:*:*:*:wrong\n" + entry, False),
            ("wildcard entry authenticates", f"*:*:*:*:{escaped}", True),
            ("nonmatching entry is skipped", "other:*:*:*:wrong\n" + entry, True),
            ("password spaces are significant", entry.rstrip(), False),
        ]:
            pgpass.write_text(content)
            check(label, plain_port, "disable", password_file=pgpass, success=success)
        pgpass.write_text(entry)
        for mode in [0o640, 0o604]:
            pgpass.chmod(mode)
            check(f"insecure password file {mode:o} is ignored", plain_port, "disable", password_file=pgpass, success=False)
        pgpass.chmod(0o400)
        check("owner-only read permission is sufficient", plain_port, "disable", password_file=pgpass)
        check("PGPASSWORD preserves literal special characters", plain_port, "disable", env_password=password)
        check("empty PGPASSWORD falls back to password file", plain_port, "disable", password_file=pgpass, env_password="")
        check("wrong PGPASSWORD takes precedence over correct file", plain_port, "disable", password_file=pgpass,
              env_password="wrong", success=False)
        pgpass.chmod(0o600)
        pgpass.write_text("*:*:*:*:wrong")
        check("PGPASSWORD takes precedence over wrong file", plain_port, "disable", password_file=pgpass,
              env_password=password)
        check("explicit password overrides environment and file", plain_port, "disable", password_file=pgpass,
              env_password="wrong", explicit_password=password)
        check("wrong explicit password does not fall back to environment", plain_port, "disable", env_password=password,
              explicit_password="wrong", success=False)
        print("All CLI TLS and password-file checks passed.", flush=True)
finally:
    for name in containers:
        run("docker", "rm", "--force", name, ok=False)
