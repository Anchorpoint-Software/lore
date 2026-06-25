# SPDX-FileCopyrightText: 2026 Anchorpoint Software GmbH
# SPDX-License-Identifier: MIT
import logging

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_read_by_path(new_lore_repo):
    repo: Lore = new_lore_repo("ReadByPath")
    content = "anchor-read-token line one\nanchor-read-token line two\n"
    repo.write_commit_push("Add file to read", {"read-me.txt": content}, offline=True)

    output = repo.read(path="read-me.txt", offline=True)
    assert "anchor-read-token line two" in output, (
        f"read by path did not return the committed content\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_read_at_revision(new_lore_repo):
    repo: Lore = new_lore_repo("ReadAtRevision")
    file = "versioned.txt"
    repo.write_commit_push("First revision", {file: "rev-one-payload\n"}, offline=True)
    repo.write_commit_push("Second revision", {file: "rev-two-payload\n"}, offline=True)

    # No revision specifier reads the current revision.
    current = repo.read(path=file, offline=True)
    assert "rev-two-payload" in current, (
        f"current read should be v2\nOutput:\n{current}"
    )
    assert "rev-one-payload" not in current, (
        f"current read should not contain v1\nOutput:\n{current}"
    )

    # An older revision specifier reads that revision's content.
    older = repo.read(path=file, revision="@1", offline=True)
    assert "rev-one-payload" in older, f"read at @1 should be v1\nOutput:\n{older}"
    assert "rev-two-payload" not in older, (
        f"read at @1 should not contain v2\nOutput:\n{older}"
    )


@pytest.mark.smoke
def test_read_missing_file_fails(new_lore_repo):
    repo: Lore = new_lore_repo("ReadMissing")
    repo.write_commit_push(
        "Add present file", {"present.txt": "present\n"}, offline=True
    )

    output = repo.read(path="does-not-exist.txt", offline=True, check=False)
    assert "file not found" in output, (
        f"reading a missing file should report failure\nOutput:\n{output}"
    )
    assert "does-not-exist.txt" in output, (
        f"the error should name the missing path\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_read_empty_file(new_lore_repo):
    repo: Lore = new_lore_repo("ReadEmpty")
    repo.write_commit_push("Add empty file", {"empty.txt": ""}, offline=True)

    output = repo.read(path="empty.txt", offline=True)
    assert output == "", (
        f"reading an empty file should yield no content\nOutput:\n{output!r}"
    )
