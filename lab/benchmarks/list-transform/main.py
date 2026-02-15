nums = list(range(1, 1_000_001))
squared = [(x * x) for x in nums]
modded = [(x % 1000) for x in squared]

total = 0
for x in modded:
    total += x

print(total)
