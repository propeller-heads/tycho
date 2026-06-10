#!/usr/bin/env bash
# Script to verify that RPC nodes support all Tycho Indexer requirements

# Don't exit on error - we want to run all tests even if some fail
set +e

# Color codes for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Variables to track test results (bash 3.2 compatible)
test_names=()
test_statuses=()
test_errors=()

# Variables for batch testing
NODES_FILE=""
BATCH_MODE=false

# Function to store test result
store_result() {
    local name=$1
    local status=$2
    local error=$3

    test_names+=("$name")
    test_statuses+=("$status")
    test_errors+=("$error")
}

# Function to get test result
get_result() {
    local name=$1
    for i in "${!test_names[@]}"; do
        if [ "${test_names[$i]}" = "$name" ]; then
            echo "${test_statuses[$i]}"
            return
        fi
    done
}

# Function to get test error
get_error() {
    local name=$1
    for i in "${!test_names[@]}"; do
        if [ "${test_names[$i]}" = "$name" ]; then
            echo "${test_errors[$i]}"
            return
        fi
    done
}

# Function to clear test results
clear_results() {
    test_names=()
    test_statuses=()
    test_errors=()
}

# Function to get node version
get_node_version() {
    local url=$1

    # Try web3_clientVersion first
    response=$(curl -s -X POST "$url" \
        -H "Content-Type: application/json" \
        -d '{
            "jsonrpc": "2.0",
            "method": "web3_clientVersion",
            "params": [],
            "id": 1
        }')

    version=$(echo "$response" | jq -r '.result // empty')

    if [ -n "$version" ] && [ "$version" != "null" ]; then
        echo "$version"
        return 0
    fi

    # If that fails, try net_version
    response=$(curl -s -X POST "$url" \
        -H "Content-Type: application/json" \
        -d '{
            "jsonrpc": "2.0",
            "method": "net_version",
            "params": [],
            "id": 1
        }')

    version=$(echo "$response" | jq -r '.result // empty')

    if [ -n "$version" ] && [ "$version" != "null" ]; then
        echo "Network version: $version"
        return 0
    fi

    echo "Unknown"
    return 1
}

# Function to test an RPC endpoint
test_endpoint() {
    local name=$1
    local url=$2
    local request=$3
    local description=$4

    if [ "$BATCH_MODE" = false ]; then
        echo -e "\n${BLUE}Testing: $description${NC}"
    fi

    response=$(curl -s -X POST "$url" \
        -H "Content-Type: application/json" \
        -d "$request")

    if echo "$response" | jq -e '.error' > /dev/null 2>&1; then
        error_msg=$(echo "$response" | jq -r '.error.message // .error')
        if [ "$BATCH_MODE" = false ]; then
            echo -e "${RED}✗ FAILED: $error_msg${NC}"
        fi
        store_result "$name" "FAILED" "$error_msg"
        return 1
    elif echo "$response" | jq -e '.result' > /dev/null 2>&1; then
        if [ "$BATCH_MODE" = false ]; then
            echo -e "${GREEN}✓ PASSED${NC}"
        fi
        store_result "$name" "PASSED" ""
        return 0
    else
        if [ "$BATCH_MODE" = false ]; then
            echo -e "${RED}✗ FAILED: Unexpected response format${NC}"
        fi
        store_result "$name" "FAILED" "Unexpected response"
        return 1
    fi
}

# Function to run all tests for a single node
test_single_node() {
    local rpc_url=$1
    local trace_rpc_url=$2

    # Use RPC_URL for TRACE_RPC_URL if not provided
    if [ -z "$trace_rpc_url" ]; then
        trace_rpc_url=$rpc_url
    fi

    if [ "$BATCH_MODE" = false ]; then
        echo -e "\n${YELLOW}RPC_URL:${NC} $rpc_url"
        if [ "$trace_rpc_url" != "$rpc_url" ]; then
            echo -e "${YELLOW}TRACE_RPC_URL:${NC} $trace_rpc_url"
        else
            echo -e "${YELLOW}TRACE_RPC_URL:${NC} Not provided (will use RPC_URL for all tests)"
        fi
    fi

    # Get node version
    if [ "$BATCH_MODE" = false ]; then
        echo -e "\n${BLUE}Getting node version...${NC}"
    fi
    node_version=$(get_node_version "$rpc_url")
    if [ "$BATCH_MODE" = false ]; then
        echo -e "${YELLOW}Node Version:${NC} $node_version"
    fi

    if [ "$BATCH_MODE" = false ]; then
        echo -e "\n${YELLOW}========================================${NC}"
        echo -e "${YELLOW}  Testing TRACE_RPC_URL Endpoints${NC}"
        echo -e "${YELLOW}========================================${NC}"
    fi

    # Test 1: eth_createAccessList with overrides (TRACE_RPC_URL)
    test_endpoint "eth_createAccessList_overrides" "$trace_rpc_url" '{
        "jsonrpc": "2.0",
        "method": "eth_createAccessList",
        "params": [
            {
                "from": "0x0000000000000000000000000000000000000001",
                "to": "0x0000000000000000000000000000000000000002",
                "value": "0xde0b6b3a7640000"
            },
            "latest",
            {
                "0x0000000000000000000000000000000000000001": {
                    "balance": "0xffffffffffffffffffffffffffffffff"
                }
            }
        ],
        "id": 1
    }' "eth_createAccessList with state overrides"

    # Test 2: debug_traceCall with prestateTracer and stateOverrides (TRACE_RPC_URL)
    test_endpoint "debug_traceCall_overrides" "$trace_rpc_url" '{
        "jsonrpc": "2.0",
        "method": "debug_traceCall",
        "params": [
            {
                "from": "0x0000000000000000000000000000000000000001",
                "to": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "data": "0x70a08231000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            },
            "latest",
            {
                "enableReturnData": true,
                "tracer": "prestateTracer",
                "stateOverrides": {
                    "0x0000000000000000000000000000000000000001": {
                        "balance": "0xde0b6b3a7640000"
                    }
                }
            }
        ],
        "id": 2
    }' "debug_traceCall with prestateTracer and stateOverrides"

    if [ "$BATCH_MODE" = false ]; then
        echo -e "\n${YELLOW}========================================${NC}"
        echo -e "${YELLOW}  Testing RPC_URL Endpoints${NC}"
        echo -e "${YELLOW}========================================${NC}"
    fi

    # Test 3: trace_callMany (RPC_URL)
    test_endpoint "trace_callMany" "$rpc_url" '{
        "jsonrpc": "2.0",
        "method": "trace_callMany",
        "params": [
            [
                [
                    {
                        "from": "0x0000000000000000000000000000000000000000",
                        "to": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                        "data": "0x70a08231000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
                    },
                    ["trace"]
                ]
            ],
            "latest"
        ],
        "id": 3
    }' "trace_callMany for transaction simulation"

    # Test 4: debug_storageRangeAt (RPC_URL)
    # First get the latest block with full transaction details, then extract a contract address from logs
    if [ "$BATCH_MODE" = false ]; then
        echo -e "\n${BLUE}Getting latest block and finding a contract address for debug_storageRangeAt test...${NC}"
    fi
    latest_block_response=$(curl -s -X POST "$rpc_url" \
        -H "Content-Type: application/json" \
        -d '{
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": ["latest", true],
            "id": 999
        }')

    block_hash=$(echo "$latest_block_response" | jq -r '.result.hash // empty')

    if [ "$BATCH_MODE" = false ]; then
        if [ -n "$block_hash" ] && [ "$block_hash" != "null" ]; then
            echo -e "${YELLOW}Latest block hash:${NC} $block_hash"
        fi
    fi

    # Try to find a contract address from the block's transactions
    contract_address=""

    # First, try to find a contract address from transaction receipts
    tx_hashes=$(echo "$latest_block_response" | jq -r '.result.transactions[].hash // empty' | head -n 10)

    for tx_hash in $tx_hashes; do
        if [ -z "$tx_hash" ] || [ "$tx_hash" = "null" ]; then
            continue
        fi

        # Get transaction receipt to find contract addresses in logs
        receipt_response=$(curl -s -X POST "$rpc_url" \
            -H "Content-Type: application/json" \
            -d "{
                \"jsonrpc\": \"2.0\",
                \"method\": \"eth_getTransactionReceipt\",
                \"params\": [\"$tx_hash\"],
                \"id\": 998
            }")

        # Try to extract address from logs (the address field in logs is usually a contract)
        log_address=$(echo "$receipt_response" | jq -r '.result.logs[0].address // empty')

        if [ -n "$log_address" ] && [ "$log_address" != "null" ] && [ "$log_address" != "0x0000000000000000000000000000000000000000" ]; then
            # Verify this address has code (is a contract)
            code_response=$(curl -s -X POST "$rpc_url" \
                -H "Content-Type: application/json" \
                -d "{
                    \"jsonrpc\": \"2.0\",
                    \"method\": \"eth_getCode\",
                    \"params\": [\"$log_address\", \"latest\"],
                    \"id\": 997
                }")

            code=$(echo "$code_response" | jq -r '.result // empty')

            # Check if it has code (more than just "0x")
            if [ -n "$code" ] && [ "$code" != "null" ] && [ "$code" != "0x" ]; then
                contract_address="$log_address"
                if [ "$BATCH_MODE" = false ]; then
                    echo -e "${GREEN}Found contract address from logs: $contract_address${NC}"
                fi
                break
            fi
        fi
    done

    # Fallback: use a well-known contract based on chain (WETH is usually deployed on most chains)
    # If we still don't have a contract, try common addresses
    if [ -z "$contract_address" ] || [ "$contract_address" = "null" ]; then
        if [ "$BATCH_MODE" = false ]; then
            echo -e "${YELLOW}Could not find contract from logs, trying common contract addresses...${NC}"
        fi

        # Common WETH addresses on different chains
        common_addresses=(
            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"  # WETH on Ethereum Mainnet
            "0x4200000000000000000000000000000000000006"  # WETH on Base, Optimism, and other OP Stack chains
            "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1"  # WETH on Arbitrum
        )

        for addr in "${common_addresses[@]}"; do
            code_response=$(curl -s -X POST "$rpc_url" \
                -H "Content-Type: application/json" \
                -d "{
                    \"jsonrpc\": \"2.0\",
                    \"method\": \"eth_getCode\",
                    \"params\": [\"$addr\", \"latest\"],
                    \"id\": 996
                }")

            code=$(echo "$code_response" | jq -r '.result // empty')

            if [ -n "$code" ] && [ "$code" != "null" ] && [ "$code" != "0x" ]; then
                contract_address="$addr"
                if [ "$BATCH_MODE" = false ]; then
                    echo -e "${GREEN}Using common contract address: $contract_address${NC}"
                fi
                break
            fi
        done
    fi

    # Final fallback: use a zero address (this might fail, but at least tests the endpoint)
    if [ -z "$contract_address" ] || [ "$contract_address" = "null" ]; then
        if [ "$BATCH_MODE" = false ]; then
            echo -e "${YELLOW}Warning: Could not find a contract address, using fallback (test may fail)${NC}"
        fi
        contract_address="0x0000000000000000000000000000000000000001"
    fi

    if [ -z "$block_hash" ] || [ "$block_hash" = "null" ]; then
        if [ "$BATCH_MODE" = false ]; then
            echo -e "${RED}✗ Could not retrieve latest block hash, using fallback test${NC}"
        fi
        # Use a known old block hash as fallback
        block_hash="0x0000000000000000000000000000000000000000000000000000000000000001"
    fi

    test_endpoint "debug_storageRangeAt" "$rpc_url" "{
        \"jsonrpc\": \"2.0\",
        \"method\": \"debug_storageRangeAt\",
        \"params\": [
            \"$block_hash\",
            0,
            \"$contract_address\",
            \"0x0000000000000000000000000000000000000000000000000000000000000000\",
            10
        ],
        \"id\": 4
    }" "debug_storageRangeAt for storage extraction"
}

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -f|--file)
            NODES_FILE="$2"
            BATCH_MODE=true
            shift 2
            ;;
        -h|--help)
            echo "Usage: ./verify_node_support.sh [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  -f, --file FILE    Test multiple nodes from a file (one URL per line)"
            echo "  -h, --help         Show this help message"
            echo ""
            echo "Single node mode:"
            echo "  RPC_URL=<url> [TRACE_RPC_URL=<url>] ./verify_node_support.sh"
            echo ""
            echo "Batch mode:"
            echo "  ./verify_node_support.sh --file nodes.txt"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

# Print header
echo -e "${BLUE}============================================${NC}"
echo -e "${BLUE}  Tycho Indexer Node Requirements Check${NC}"
echo -e "${BLUE}============================================${NC}"

# Batch mode: test multiple nodes from file
if [ "$BATCH_MODE" = true ]; then
    if [ ! -f "$NODES_FILE" ]; then
        echo -e "${RED}Error: File not found: $NODES_FILE${NC}"
        exit 1
    fi

    echo -e "\n${YELLOW}Testing nodes from file: $NODES_FILE${NC}\n"

    # Arrays to store results for all nodes
    declare -a all_nodes
    declare -a all_versions
    declare -a all_results

    # Read nodes from file and test each one
    line_num=0
    while IFS= read -r node_url || [ -n "$node_url" ]; do
        # Skip empty lines and comments
        [[ -z "$node_url" || "$node_url" =~ ^[[:space:]]*# ]] && continue

        line_num=$((line_num + 1))

        echo -e "${BLUE}[$line_num] Testing: $node_url${NC}"

        # Clear previous results
        clear_results

        # Get node version first
        node_version=$(get_node_version "$node_url")

        # Test the node (suppress stdout to avoid capturing test output)
        test_single_node "$node_url" "" > /dev/null 2>&1

        # Store results
        all_nodes+=("$node_url")
        all_versions+=("$node_version")

        # Count passed/failed for this node
        passed=0
        failed=0
        for status in "${test_statuses[@]}"; do
            if [ "$status" = "PASSED" ]; then
                ((passed++))
            else
                ((failed++))
            fi
        done

        # Store test results as a string
        result_str="$passed/$((passed+failed))"
        all_results+=("$result_str")

        # Store individual endpoint results
        eth_create=$(get_result "eth_createAccessList_overrides")
        debug_trace=$(get_result "debug_traceCall_overrides")
        trace_call=$(get_result "trace_callMany")
        debug_storage=$(get_result "debug_storageRangeAt")

        # Create a result summary
        result_summary="${eth_create:0:1}${debug_trace:0:1}${trace_call:0:1}${debug_storage:0:1}"
        all_results[$((${#all_results[@]}-1))]="$result_str|$result_summary"

        if [ $failed -eq 0 ]; then
            echo -e "${GREEN}✓ All tests passed${NC}\n"
        else
            echo -e "${YELLOW}⚠ $failed test(s) failed${NC}\n"
        fi
    done < "$NODES_FILE"

    # Print summary report
    echo -e "${BLUE}============================================${NC}"
    echo -e "${BLUE}         Batch Test Summary Report${NC}"
    echo -e "${BLUE}============================================${NC}\n"

    echo -e "${YELLOW}TRACE_RPC_URL Endpoints (positions 1-2):${NC}"
    printf "%-50s %-30s %-15s %-10s\n" "Node URL" "Version" "TRACE Pass/Total" "Status"
    printf "%-50s %-30s %-15s %-10s\n" "--------" "-------" "----------------" "------"

    for i in "${!all_nodes[@]}"; do
        node_url="${all_nodes[$i]}"
        version="${all_versions[$i]}"
        result="${all_results[$i]}"

        # Truncate long URLs
        if [ ${#node_url} -gt 48 ]; then
            display_url="${node_url:0:45}..."
        else
            display_url="$node_url"
        fi

        # Truncate long versions
        if [ ${#version} -gt 28 ]; then
            display_version="${version:0:25}..."
        else
            display_version="$version"
        fi

        # Split result into pass/total and endpoint results
        IFS='|' read -r pass_total endpoints <<< "$result"

        # Extract TRACE endpoints (positions 1-2)
        trace_endpoints="${endpoints:0:2}"
        trace_passed=0
        trace_total=2

        for ((j=0; j<${#trace_endpoints}; j++)); do
            if [ "${trace_endpoints:$j:1}" = "P" ]; then
                ((trace_passed++))
            fi
        done

        # Color code the TRACE endpoints
        trace_display=""
        for ((j=0; j<${#trace_endpoints}; j++)); do
            char="${trace_endpoints:$j:1}"
            if [ "$char" = "P" ]; then
                trace_display+="✓"
            else
                trace_display+="✗"
            fi
        done

        printf "%-50s %-30s %-15s %-10s\n" "$display_url" "$display_version" "$trace_passed/$trace_total" "$trace_display"
    done

    echo -e "\n${YELLOW}RPC_URL Endpoints (positions 3-4):${NC}"
    printf "%-50s %-30s %-15s %-10s\n" "Node URL" "Version" "RPC Pass/Total" "Status"
    printf "%-50s %-30s %-15s %-10s\n" "--------" "-------" "--------------" "------"

    for i in "${!all_nodes[@]}"; do
        node_url="${all_nodes[$i]}"
        version="${all_versions[$i]}"
        result="${all_results[$i]}"

        # Truncate long URLs
        if [ ${#node_url} -gt 48 ]; then
            display_url="${node_url:0:45}..."
        else
            display_url="$node_url"
        fi

        # Truncate long versions
        if [ ${#version} -gt 28 ]; then
            display_version="${version:0:25}..."
        else
            display_version="$version"
        fi

        # Split result into pass/total and endpoint results
        IFS='|' read -r pass_total endpoints <<< "$result"

        # Extract RPC endpoints (positions 3-4)
        rpc_endpoints="${endpoints:2:2}"
        rpc_passed=0
        rpc_total=2

        for ((j=0; j<${#rpc_endpoints}; j++)); do
            if [ "${rpc_endpoints:$j:1}" = "P" ]; then
                ((rpc_passed++))
            fi
        done

        # Color code the RPC endpoints
        rpc_display=""
        for ((j=0; j<${#rpc_endpoints}; j++)); do
            char="${rpc_endpoints:$j:1}"
            if [ "$char" = "P" ]; then
                rpc_display+="✓"
            else
                rpc_display+="✗"
            fi
        done

        printf "%-50s %-30s %-15s %-10s\n" "$display_url" "$display_version" "$rpc_passed/$rpc_total" "$rpc_display"
    done

    echo -e "\n${YELLOW}Combined Analysis:${NC}"
    echo -e "You can combine nodes where one passes all TRACE tests (2/2) and another passes all RPC tests (2/2)\n"

    # Find nodes that pass all TRACE tests
    echo -e "${GREEN}Nodes with full TRACE_RPC_URL support (✓✓):${NC}"
    has_trace_support=false
    for i in "${!all_nodes[@]}"; do
        result="${all_results[$i]}"
        IFS='|' read -r pass_total endpoints <<< "$result"
        trace_endpoints="${endpoints:0:2}"
        if [ "$trace_endpoints" = "PP" ]; then
            has_trace_support=true
            node_url="${all_nodes[$i]}"
            if [ ${#node_url} -gt 70 ]; then
                echo "  - ${node_url:0:67}..."
            else
                echo "  - $node_url"
            fi
        fi
    done
    if [ "$has_trace_support" = false ]; then
        echo "  (none)"
    fi

    # Find nodes that pass all RPC tests
    echo -e "\n${GREEN}Nodes with full RPC_URL support (✓✓):${NC}"
    has_rpc_support=false
    for i in "${!all_nodes[@]}"; do
        result="${all_results[$i]}"
        IFS='|' read -r pass_total endpoints <<< "$result"
        rpc_endpoints="${endpoints:2:2}"
        if [ "$rpc_endpoints" = "PP" ]; then
            has_rpc_support=true
            node_url="${all_nodes[$i]}"
            if [ ${#node_url} -gt 70 ]; then
                echo "  - ${node_url:0:67}..."
            else
                echo "  - $node_url"
            fi
        fi
    done
    if [ "$has_rpc_support" = false ]; then
        echo "  (none)"
    fi

    # Find nodes that pass all tests
    echo -e "\n${GREEN}Nodes with full support (can be used for both TRACE_RPC_URL and RPC_URL):${NC}"
    has_full_support=false
    for i in "${!all_nodes[@]}"; do
        result="${all_results[$i]}"
        IFS='|' read -r pass_total endpoints <<< "$result"
        if [ "$endpoints" = "PPPP" ]; then
            has_full_support=true
            node_url="${all_nodes[$i]}"
            if [ ${#node_url} -gt 70 ]; then
                echo "  - ${node_url:0:67}..."
            else
                echo "  - $node_url"
            fi
        fi
    done
    if [ "$has_full_support" = false ]; then
        echo "  (none)"
    fi

    echo -e "\n${YELLOW}Endpoint Legend:${NC}"
    echo -e "${BLUE}TRACE_RPC_URL endpoints:${NC}"
    echo -e "  Position 1: eth_createAccessList with overrides"
    echo -e "  Position 2: debug_traceCall with overrides"
    echo -e "${BLUE}RPC_URL endpoints:${NC}"
    echo -e "  Position 3: trace_callMany"
    echo -e "  Position 4: debug_storageRangeAt"
    echo -e "\n  ${GREEN}✓${NC} = Supported, ${RED}✗${NC} = Not Supported"

    exit 0
fi

# Single node mode: validate environment variables
if [ -z "$RPC_URL" ]; then
    echo -e "${RED}Error: RPC_URL environment variable is not set${NC}"
    echo "Usage: RPC_URL=<url> [TRACE_RPC_URL=<url>] ./verify_node_support.sh"
    echo "   or: ./verify_node_support.sh --file nodes.txt"
    exit 1
fi

# Run tests for single node
test_single_node "$RPC_URL" "$TRACE_RPC_URL"

# Print summary
echo -e "\n${BLUE}============================================${NC}"
echo -e "${BLUE}           Test Results Summary${NC}"
echo -e "${BLUE}============================================${NC}\n"

echo -e "${YELLOW}TRACE_RPC_URL endpoints:${NC}"
result=$(get_result "eth_createAccessList_overrides")
if [ "$result" = "PASSED" ]; then
    echo -e "  ${GREEN}✓${NC} eth_createAccessList with overrides: ${GREEN}SUPPORTED${NC}"
else
    echo -e "  ${RED}✗${NC} eth_createAccessList with overrides: ${RED}NOT SUPPORTED${NC}"
    error=$(get_error "eth_createAccessList_overrides")
    [ -n "$error" ] && echo -e "    Error: $error"
fi

result=$(get_result "debug_traceCall_overrides")
if [ "$result" = "PASSED" ]; then
    echo -e "  ${GREEN}✓${NC} debug_traceCall with overrides: ${GREEN}SUPPORTED${NC}"
else
    echo -e "  ${RED}✗${NC} debug_traceCall with overrides: ${RED}NOT SUPPORTED${NC}"
    error=$(get_error "debug_traceCall_overrides")
    [ -n "$error" ] && echo -e "    Error: $error"
fi

echo -e "\n${YELLOW}RPC_URL endpoints:${NC}"
result=$(get_result "trace_callMany")
if [ "$result" = "PASSED" ]; then
    echo -e "  ${GREEN}✓${NC} trace_callMany: ${GREEN}SUPPORTED${NC}"
else
    echo -e "  ${RED}✗${NC} trace_callMany: ${RED}NOT SUPPORTED${NC}"
    error=$(get_error "trace_callMany")
    [ -n "$error" ] && echo -e "    Error: $error"
fi

result=$(get_result "debug_storageRangeAt")
if [ "$result" = "PASSED" ]; then
    echo -e "  ${GREEN}✓${NC} debug_storageRangeAt: ${GREEN}SUPPORTED${NC}"
else
    echo -e "  ${RED}✗${NC} debug_storageRangeAt: ${RED}NOT SUPPORTED${NC}"
    error=$(get_error "debug_storageRangeAt")
    [ -n "$error" ] && echo -e "    Error: $error"
fi

# Count passed/failed tests
passed=0
failed=0
for status in "${test_statuses[@]}"; do
    if [ "$status" = "PASSED" ]; then
        ((passed++))
    else
        ((failed++))
    fi
done

echo -e "\n${BLUE}============================================${NC}"
if [ $failed -eq 0 ]; then
    echo -e "${GREEN}✓ All tests passed ($passed/$((passed+failed)))${NC}"
    echo -e "${GREEN}This node meets all Tycho Indexer requirements!${NC}"
    exit 0
else
    echo -e "${RED}✗ Some tests failed ($failed/$((passed+failed)) failed)${NC}"
    echo -e "${YELLOW}This node may not fully support Tycho Indexer.${NC}"
    exit 1
fi
