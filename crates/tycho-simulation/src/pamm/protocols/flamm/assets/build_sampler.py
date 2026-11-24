#!/usr/bin/env python3
"""Rebuild FlammSampler (assets/) and the test pool MaxOfTwoPool (testdata/) with solc 0.8.35; --check verifies the
committed runtimes. Both are written as raw runtime bytes, never hex: a node runs the sampler by a state override, and
the tests set the pool's code at an address."""
import argparse
import json
import pathlib
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--solc', default='solc')
parser.add_argument('--check', action='store_true')
args = parser.parse_args()
version = subprocess.check_output([args.solc, '--version'], text=True)
assert 'Version: 0.8.35+commit.47b9dedd' in version, version
module = pathlib.Path(__file__).resolve().parent.parent
settings = {'viaIR': True, 'optimizer': {'enabled': True, 'runs': 1}, 'evmVersion': 'paris',
            'metadata': {'bytecodeHash': 'none', 'appendCBOR': False},
            'outputSelection': {'*': {'*': ['abi', 'evm.deployedBytecode.object']}}}
outputs = {}
for source in (module / 'assets/FlammSampler.sol', module / 'testdata/MaxOfTwoPool.sol'):
    request = {'language': 'Solidity', 'sources': {source.name: {'content': source.read_text()}}, 'settings': settings}
    process = subprocess.run([args.solc, '--standard-json'], input=json.dumps(request), text=True, capture_output=True,
                             check=True)
    result = json.loads(process.stdout)
    for error in result.get('errors', []):
        print(error['formattedMessage'])
    assert not any(error['severity'] == 'error' for error in result.get('errors', []))
    contract = result['contracts'][source.name][source.stem]
    outputs[source.with_suffix('.evm.runtime')] = bytes.fromhex(contract['evm']['deployedBytecode']['object'])
for path, content in outputs.items():
    if args.check:
        assert path.read_bytes() == content, f'{path.name} is stale; run without --check'
    else:
        path.write_bytes(content)
print('Sampler runtimes verified' if args.check else 'Sampler runtimes rebuilt')
