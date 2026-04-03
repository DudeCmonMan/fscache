// Phase 2: LRU cache manager
// Tracks cached files using filesystem timestamps (no persistent DB).
// Evicts files older than expiry_hours or when max_size_gb is exceeded.
