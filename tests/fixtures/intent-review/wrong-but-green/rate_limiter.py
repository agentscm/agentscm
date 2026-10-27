from collections import defaultdict


class RateLimiter:
    """Incorrect: each client IP gets an independent counter."""

    def __init__(self):
        self.counts = defaultdict(int)

    def allow(self, account_id, client_ip, now):
        window = now // 60
        self.counts[(client_ip, window)] += 1
        return self.counts[(client_ip, window)] <= 2
