Write a file, creating missing folders, or replace an existing file completely. To replace an existing file, read it first in this session; if it changed since you read it, read it again. Prefer edit for changing part of a file.
- file_path: the path; a relative path is taken from your worktree.
- content: the whole new content.
Writes outside your worktree, the evidence folder and the system temporary folder are refused.
