// Phase 2: Background SMB → SSD file copy
// Writes to {path}.partial during copy, atomic rename on completion.
// FUSE ignores .partial files so reads pass through to backing store until done.
