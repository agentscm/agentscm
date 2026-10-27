from collections import defaultdict


class RateLimiter:
    """Fixed-window example keyed by account, independent of client IP."""

    def __init__(self):
        self.counts = defaultdict(int)

    def allow(self, account_id, client_ip, now):
        window = now // 60
        key = (account_id, window)
        self.counts[key] += 1
        return self.counts[key] <= 2
