# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""`--local` reads answer from local data without waiting on a remote connect.

The reads run against an endpoint that holds the port and never answers. A read
that awaits the connect there does not return for minutes, so returning inside
`LOCAL_READ_DEADLINE_S` is only possible without awaiting it. Each read is also
checked against the local latest, so a fast wrong answer fails too.
"""

import logging
import socket
import time
from contextlib import contextmanager

import pytest

from lore_server import (
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)
from test_branch_switch_reconnect import _force_kill_server

logger = logging.getLogger(__name__)

# A read that awaits the connect takes minutes against the silent endpoint, so
# the bound only has to be clear of local work.
LOCAL_READ_DEADLINE_S = 3.0

# The listening sockets are released as the killed server is reaped, which the
# kernel may not have finished when the bind is attempted.
BIND_TIMEOUT_S = 10.0


def _bind_with_retry(sock: socket.socket, port: int) -> None:
    """Bind `sock` to `port` on loopback, waiting out the release of the killed
    server's listeners.

    Retried per socket: the server's UDP and TCP listeners are not released
    together, so a bind that has already succeeded must not be attempted again.
    """
    deadline = time.monotonic() + BIND_TIMEOUT_S
    while True:
        try:
            sock.bind(("127.0.0.1", port))
            return
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.1)


@contextmanager
def _silent_endpoint(port: int):
    """Hold `port` on loopback with sockets that accept traffic and never answer.

    A connect against a closed port is refused in microseconds, which is not the
    failure being guarded here: the cost comes from a connect whose packets go
    unanswered until the transport gives up. Binding the port without serving it
    reproduces that, so a read that returns promptly can only have skipped the
    connect. Both protocols are held because the port carries QUIC and gRPC.
    """
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        for held in (udp, tcp):
            held.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            _bind_with_retry(held, port)
        tcp.listen(8)
        yield
    finally:
        udp.close()
        tcp.close()


def _timed(label: str, deadline_s: float, fn):
    start = time.monotonic()
    result = fn()
    elapsed = time.monotonic() - start
    logger.info("%s completed in %.2fs", label, elapsed)
    assert elapsed < deadline_s, (
        f"{label} took {elapsed:.2f}s against a silent endpoint — "
        "it is still driving a needless remote connect"
    )
    return result


@pytest.mark.smoke
def test_local_reads_skip_remote_connect(
    request,
    tmp_path_factory,
    lore_server_executable_path,
    new_lore_repo,
):
    # Dedicated server for this test so killing it doesn't disrupt tests
    # that share the session-scoped autouse server. Mirrors the pattern in
    # scripts/test/test_branch_switch_reconnect.py.
    shared_port = allocate_free_port()
    server_ports = {
        "quic": shared_port,
        "grpc": shared_port,
        "http": allocate_free_port(),
        "internal": allocate_free_port(),
    }
    server_root, server_env = generate_server_config(
        request, tmp_path_factory, server_ports
    )
    server_proc, _server_log_path, server_log_fd = launch_lore_server(
        server_root, server_env, lore_server_executable_path
    )
    try:
        repo = new_lore_repo(remote_url=f"lore://127.0.0.1:{shared_port}/")
        text_file = "file.txt"
        repo.write_commit_push("Initial commit", {text_file: ["Line one\n"]})
        local_latest = repo.revision_history(1, offline=True)[0].signature
    finally:
        _force_kill_server(server_proc, server_log_fd)

    with _silent_endpoint(shared_port):
        # Explicit-branch history: --local forbids remote traffic outright.
        revisions = _timed(
            "history(branch=main, --local)",
            LOCAL_READ_DEADLINE_S,
            lambda: repo.history(1, branch="main", local=True),
        )
        assert revisions and revisions[0].signature == local_latest, (
            "--local explicit-branch history must return the local latest"
        )

        # branch@LATEST resolves through revision::resolve, which picks the
        # search location before reaching for a remote latest.
        revisions = _timed(
            "history(revision=main@LATEST, --local)",
            LOCAL_READ_DEADLINE_S,
            lambda: repo.history(1, revision="main@LATEST", local=True),
        )
        assert revisions and revisions[0].signature == local_latest, (
            "--local branch@LATEST resolve must return the local latest"
        )

        # File history takes its --local return ahead of the remote latest.
        output = _timed(
            "file history(--local)",
            LOCAL_READ_DEADLINE_S,
            lambda: repo.file_history(text_file, branch="main", local=True),
        )
        assert "Initial commit" in output, (
            "--local file history must list the committing revision"
        )

        # A partial signature that matches nothing exhausts the search, which
        # under --local means the local branches and their local latests only.
        output = _timed(
            "history(revision=<unknown partial>, --local)",
            LOCAL_READ_DEADLINE_S,
            lambda: repo.run(
                ["history", "1", "--revision", "deadbeef"], local=True, check=False
            ),
        )
        assert "deadbeef" in output, (
            "--local resolve of an unknown signature must report it as not found"
        )

        # `revision find` starts its walk from the branch latest, which --local
        # takes locally rather than from the remote.
        output = _timed(
            "revision find number(--local)",
            LOCAL_READ_DEADLINE_S,
            lambda: repo.run(
                ["revision", "find", "number", "999"], local=True, check=False
            ),
        )
        assert "No matching revision found" in output, (
            "--local find of an unknown revision number must report it as not found"
        )
