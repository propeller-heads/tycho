---
description: Tycho Client CLI installation documentation
---

# Binary / CLI

## When to use the binary client

The binary client is recommended for 2 situations:

* For a quick setup, to consume data from Tycho Indexer direct on a terminal
* To consume data from Tycho Indexer on apps developed in languages where there isn't a native tycho client available (e.g: any languages apart from Rust and Python). For the supported languages, please check the [rust-client.md](rust-client.md "mention")or [python-client.md](python-client.md "mention")docs.

## Installing Tycho-client

This guide provides two methods to install Tycho Client:

1. Install with Cargo (recommended for most users)
2. Download pre-built binaries from GitHub Releases

### Method 1: Install with Cargo

#### Prerequisites

* Cargo
* Rust 1.84.0 or later

```
cargo install tycho-client
```

### Method 2: Download from GitHub Releases

**Step 1: Download the pre-built binary**

For a simple, setup-free start, download the latest `tycho-client` binary release that matches your OS/architecture on <a href="https://github.com/propeller-heads/tycho-indexer/releases" target="_blank" rel="noopener noreferrer">GitHub</a>.

{% hint style="info" %}
> 💡 **Tip**: Choose the latest release unless you need a specific version.
{% endhint %}

**Step 2:  Extract the binary from the tar.gz**

Open a terminal and navigate to the directory where the file was downloaded. Run the following command to extract the contents:

```bash
tar -xvzf tycho-client-aarch64-apple-darwin-{version}.tar.gz
```

**Step 3: Link the binary to a directory in your system's PATH (recommended):**

```bash
// Ensure the binary is executable:
sudo chmod +x tycho-client
// Create symlink
sudo ln -s $(pwd)/tycho-client /usr/local/bin/tycho-client
```

<details>

<summary>Additional info on adding to PATH</summary>

NOTE: This command requires `/usr/local/bin` to be included in the system's `PATH.` While this is typically the case, there may be exceptions.

If `/usr/local/bin` is not in your `PATH`, you can either:

1. Add it to your `PATH` by exporting it:

```bash
export PATH"/usr/local/bin:$PATH"
```

2. Or create a symlink in any of the following directories (if they are in your `PATH`):

```
/bin
/sbin
/usr/bin
/usr/sbin
/usr/local/bin
/usr/local/sbin
```

</details>

**Step 4: Verify Installation**

```bash
tycho-client --version
tycho-client 0.54.0 # should match the latest version published on GitHub
```

You should see the Tycho Client version displayed. If you need more guidance, contact us via <a href="https://t.me/c/2288091950/1" target="_blank" rel="noopener noreferrer">Telegram</a>

***

## Using Tycho Client

### Running the client

#### Step 1: Setting up API Key

If you're connecting to our hosted service, please follow our [#authentication](./#authentication "mention") to get an API Key. Once you have a key, export it using an environment variable

```bash
export TYCHO_AUTH_TOKEN={your_token}
```

or use the command line flag

```bash
tycho-client --auth-key {your_token}
```

#### **Step 2: Consume data from Tycho Indexer**

Now, you're all set up!

Before consuming the data, you first need to choose which protocols you want to track. You can find a list of[#available-protocols](../../hosted-endpoints.md#available-protocols "mention") here. \
For example, to track the Uniswap V2 and V3 pools on Mainnet, with a minimum value locked of 100 ETH, run:

```bash
tycho-client --exchange uniswap_v2 --exchange uniswap_v3 --min-tvl 100 --tycho-url 
tycho-beta.propellerheads.xyz
```

Or skip secure connections entirely with `--no-tls` for local setups \[coming soon].

### Debugging

Since all messages are sent directly to stdout in a single line, logs are saved to a file: `./logs/dev_logs.log`. You can configure the directory with the `--log-dir` option.

### Configuring the client

For more details on using the CLI and its parameters, run:

```bash
tycho client --help
```

For extended explanation on how each parameter works, check our [#usage](./#usage "mention")guide.
