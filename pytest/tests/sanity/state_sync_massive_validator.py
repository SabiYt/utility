#!/usr/bin/env python3
"""
Survive massive state sync for validator

Create 4 nodes, 3 validators and 1 observers tracking the single shard 0.
Generate a large state using genesis-populate. [*]

Spawn everything and wait for them to make some progress.
Kill one of the validators, delete state and rerun it

To run this test is important to compile genesis-populate tool first.
In framework folder run:

```
cargo build -p genesis-populate
```

[*] This test might take a very large time generating the state.
To speed up this between multiple executions, large state can be generated once
saved, and reused on multiple executions. Steps to do this.

1. Run test for first time:

```
python3 tests/sanity/state_sync_massive_validator.py
```

Stop at any point after seeing the message: "Genesis generated"

2. Save generated data:

```
cp -r ~/.unc/test0_finished ~/.unc/backup_genesis
```

3. Run test passing path to backup_genesis

```
python3 tests/sanity/state_sync_massive_validator.py ~/.unc/backup_genesis
```
"""

from subprocess import check_output
import logging
import pathlib
import requests
import sys
import time

sys.path.append(str(pathlib.Path(__file__).resolve().parents[2] / 'lib'))

from cluster import init_cluster, spin_up_node, load_config
from populate import genesis_populate_all, copy_genesis
import state_sync_lib

logging.basicConfig(format='%(asctime)s %(message)s', level=logging.DEBUG)

if len(sys.argv) >= 2:
    genesis_data = sys.argv[1]
else:
    genesis_data = None
    additional_accounts = 200000

EPOCH_LENGTH = 300

config = load_config()
node_config = state_sync_lib.get_state_sync_config_combined()
unc_root, node_dirs = init_cluster(
    3, 1, 1, config,
    [["min_gas_price", 0], ["max_inflation_rate", [0, 1]],
     ["epoch_length", EPOCH_LENGTH], ["block_producer_kickout_threshold", 0],
     ["chunk_producer_kickout_threshold", 0]],
    {x: node_config for x in range(4)})

logging.info("Populating genesis")

if genesis_data is None:
    genesis_populate_all(unc_root, additional_accounts, node_dirs)
else:
    for node_dir in node_dirs:
        copy_genesis(genesis_data, node_dir)

logging.info("Genesis generated")

for node_dir in node_dirs:
    result = check_output(['ls', '-la', node_dir], text=True)
    logging.info(f'Node directory: {node_dir}')
    for line in result.split('\n'):
        logging.info(line)

INTERMEDIATE_HEIGHT = EPOCH_LENGTH + 10
SMALL_HEIGHT = EPOCH_LENGTH * 2 + 10
LARGE_HEIGHT = SMALL_HEIGHT + 50
TIMEOUT = 3600
start = time.time()

boot_node = spin_up_node(config, unc_root, node_dirs[0], 0)
validator = spin_up_node(config,
                         unc_root,
                         node_dirs[1],
                         1,
                         boot_node=boot_node)
delayed_validator = spin_up_node(config,
                                 unc_root,
                                 node_dirs[2],
                                 2,
                                 boot_node=boot_node)
observer = spin_up_node(config, unc_root, node_dirs[3], 3, boot_node=boot_node)


def wait_for_height(target_height, rpc_node, sleep_time=2, bps_threshold=-1):
    queue = []
    latest_height = 0

    while latest_height < target_height:
        assert time.time() - start < TIMEOUT

        # Check current height
        try:
            new_height = rpc_node.get_latest_block(check_storage=False,
                                                   timeout=10).height
            logging.info(f"Height: {latest_height} => {new_height}")
            latest_height = new_height
        except requests.ReadTimeout:
            logging.info("Timeout Error")

        # Computing bps
        cur_time = time.time()
        queue.append((cur_time, latest_height))

        while len(queue) > 2 and queue[0][0] <= cur_time - 7:
            queue.pop(0)

        if len(queue) <= 1:
            bps = None
        else:
            head = queue[-1]
            tail = queue[0]
            bps = (head[1] - tail[1]) / (head[0] - tail[0])

        logging.info(f"bps: {bps} queue length: {len(queue)}")
        time.sleep(sleep_time)
        assert bps is None or bps >= bps_threshold


wait_for_height(INTERMEDIATE_HEIGHT, validator)

delayed_validator.kill()
delayed_validator.reset_data()
delayed_validator.start(boot_node=boot_node)

# Check that bps is not degraded
wait_for_height(LARGE_HEIGHT, validator)

wait_for_height(SMALL_HEIGHT, delayed_validator)
