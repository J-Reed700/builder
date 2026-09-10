def expire_session_cache():
    """Remove expired session cache entries."""
    remove_expired_entries()


def cache_statistics():
    return count_entries()
