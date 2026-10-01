"""Loopback-only disposable SSH server for ssh_integration.rs (requires paramiko).

Usage: python ssh_fixture.py /path/to/new/fixture-directory
The directory should be on a Windows drive so Windows OpenSSH can read its keys.
No real SSH configuration, accounts, or credentials are used or changed.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading
import tempfile
import shutil
import re

import paramiko

root = Path(sys.argv[1]).resolve()
root.mkdir(mode=0o700)  # Refuse to overwrite existing fixtures.
for name, password in [("plain", ""), ("encrypted", "clipboard-fixture")]:
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", password,
                    "-f", str(root / name)], check=True)
accepted_keys = {(root / f"{name}.pub").read_text().split()[1] for name in ["plain", "encrypted"]}
linux_root = Path(tempfile.mkdtemp(prefix="wch-ssh-keys-"))
for name in ["plain", "encrypted"]:
    shutil.copyfile(root / name, linux_root / name)
    (linux_root / name).chmod(0o600)
host_key = paramiko.RSAKey.generate(2048)
listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen(16)
port = listener.getsockname()[1]
(root / "known_hosts").write_text(f"[127.0.0.1]:{port} {host_key.get_name()} {host_key.get_base64()}\n")
windows_root = subprocess.check_output(["wslpath", "-m", str(root)], text=True).strip()
# WSL-created NTFS files can have extra ACL entries; restrict ONLY our fixture keys.
acl_script = r'''$id = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
foreach ($name in @("plain", "encrypted")) {
    $acl = New-Object System.Security.AccessControl.FileSecurity
    $acl.SetOwner($id)
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($id, "FullControl", "Allow")))
    Set-Acl -LiteralPath (Join-Path $PSScriptRoot $name) -AclObject $acl
}
'''
(root / "fixture-acl.ps1").write_text(acl_script)
subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", f"{windows_root}/fixture-acl.ps1"], check=True)
wrong_host = paramiko.RSAKey.generate(2048)
(root / "wrong_hosts").write_text(f"[127.0.0.1]:{port} {wrong_host.get_name()} {wrong_host.get_base64()}\n")
for name, base in [("windows", windows_root), ("wsl", str(root))]:
    keys = base if name == "windows" else str(linux_root)
    config = f'''Host changed
    UserKnownHostsFile "{base}/wrong_hosts"
Host agent
    IdentityAgent SSH_AUTH_SOCK
    IdentityFile "{keys}/encrypted"
Host reused
    ControlMaster auto
    ControlPath {linux_root}/master
    ControlPersist 10m
Host *
    HostName 127.0.0.1
    Port {port}
    User fixture
    UserKnownHostsFile "{base}/known_hosts"
    StrictHostKeyChecking yes
    IdentityAgent none
    IdentitiesOnly yes
Host key
    IdentityFile "{keys}/plain"
Host encrypted
    IdentityFile "{keys}/encrypted"
Host password cancel wrong reused changed
    IdentityFile none
    PreferredAuthentications password
'''
    (root / f"{name}.conf").write_text(config)

lock = threading.Lock()
def record(kind, **fields):
    with lock, (root / "events.jsonl").open("a") as f:
        f.write(json.dumps({"kind": kind, **fields}) + "\n")

class Server(paramiko.ServerInterface):
    def get_allowed_auths(self, username):
        return "publickey,password"

    def check_auth_password(self, username, password):
        ok = username == "fixture" and password == "clipboard-fixture"
        record("password", accepted=ok)
        return paramiko.AUTH_SUCCESSFUL if ok else paramiko.AUTH_FAILED

    def check_auth_publickey(self, username, key):
        ok = username == "fixture" and key.get_base64() in accepted_keys
        record("publickey", accepted=ok)
        return paramiko.AUTH_SUCCESSFUL if ok else paramiko.AUTH_FAILED

    def check_channel_request(self, kind, chanid):
        return paramiko.OPEN_SUCCEEDED if kind == "session" else paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED

    def check_channel_exec_request(self, channel, command):
        record("exec")
        threading.Thread(target=execute, args=(channel, command), daemon=True).start()
        return True

    def check_channel_shell_request(self, channel):
        record("shell")
        threading.Thread(target=execute, args=(channel, b"sh"), daemon=True).start()
        return True

def execute(channel, command):
    proc = subprocess.Popen(["sh", "-c", command.decode()], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    def inbound():
        try:
            while chunk := channel.recv(32768):
                proc.stdin.write(chunk)
                proc.stdin.flush()
        except (OSError, EOFError):
            pass
        finally:
            try:
                proc.stdin.close()
            except OSError:
                pass

    def outbound(pipe, send):
        try:
            while chunk := os.read(pipe.fileno(), 32768):
                send(chunk)
        except (OSError, EOFError):
            pass

    threading.Thread(target=inbound, daemon=True).start()
    err = threading.Thread(target=outbound, args=(proc.stderr, channel.sendall_stderr), daemon=True)
    err.start()
    outbound(proc.stdout, channel.sendall)
    code = proc.wait()
    err.join(timeout=1)
    try:
        channel.send_exit_status(code)
        channel.close()
    except (OSError, EOFError):
        pass

def client(sock):
    transport = paramiko.Transport(sock)
    transport.add_server_key(host_key)
    try:
        transport.start_server(server=Server())
        channels = []
        while transport.is_active():
            channel = transport.accept(timeout=1)
            if channel is not None:
                channels.append(channel)
    except (OSError, EOFError, paramiko.SSHException):
        pass
    finally:
        transport.close()

def accept_clients():
    while True:
        sock, _ = listener.accept()
        threading.Thread(target=client, args=(sock,), daemon=True).start()

threading.Thread(target=accept_clients, daemon=True).start()
askpass = linux_root / "askpass"
askpass.write_text("#!/bin/sh\nprintf '%s\\n' clipboard-fixture\n")
askpass.chmod(0o700)
sock = str(linux_root / "agent")
agent = subprocess.check_output(["ssh-agent", "-a", sock, "-s"], text=True)
agent_pid = int(re.search(r"SSH_AGENT_PID=(\d+)", agent).group(1))
env = dict(os.environ, SSH_AUTH_SOCK=sock, SSH_ASKPASS=str(askpass), SSH_ASKPASS_REQUIRE="force", DISPLAY="fixture:0")
subprocess.run(["ssh-add", str(linux_root / "encrypted")], env=env, stdin=subprocess.DEVNULL, check=True)
(root / "agent-socket").write_text(sock)
subprocess.run(["ssh", "-F", str(root / "wsl.conf"), "-MNf", "reused"], env=env, stdin=subprocess.DEVNULL, check=True)
print(json.dumps({"directory": str(root), "windows_directory": windows_root, "port": port}), flush=True)
try:
    threading.Event().wait()
except KeyboardInterrupt:
    subprocess.run(["ssh", "-F", str(root / "wsl.conf"), "-O", "exit", "reused"], capture_output=True)
    os.kill(agent_pid, 15)
    listener.close()
    shutil.rmtree(linux_root)
