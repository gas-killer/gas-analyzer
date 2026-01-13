/**
 * Example: Using gas-analyzer-rs WASM in a frontend application
 * 
 * This shows how to:
 * 1. Initialize the WASM module
 * 2. Fetch a transaction trace from an RPC endpoint
 * 3. Analyze the trace for gas optimization opportunities
 */

import init, { analyze_trace, compute_state_updates_from_trace_json } from '../pkg/gas_analyzer_rs.js';

// Initialize WASM module (call this once on page load)
let wasmInitialized = false;

async function initializeWasm() {
    if (!wasmInitialized) {
        await init();
        wasmInitialized = true;
        console.log('WASM module initialized');
    }
}

/**
 * Fetch a transaction trace from an RPC endpoint
 * 
 * @param {string} rpcUrl - RPC endpoint URL
 * @param {string} txHash - Transaction hash (0x-prefixed)
 * @returns {Promise<Object>} Geth trace JSON (DefaultFrame format)
 */
async function fetchTransactionTrace(rpcUrl, txHash) {
    const response = await fetch(rpcUrl, {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
        },
        body: JSON.stringify({
            jsonrpc: '2.0',
            method: 'debug_traceTransaction',
            params: [
                txHash,
                {
                    enableMemory: true,
                    // Use default tracer to get struct_logs format
                    // tracer: undefined means default tracer
                }
            ],
            id: 1,
        }),
    });

    if (!response.ok) {
        throw new Error(`RPC request failed: ${response.statusText}`);
    }

    const data = await response.json();
    if (data.error) {
        throw new Error(`RPC error: ${data.error.message}`);
    }

    // The result should be in DefaultFrame format with struct_logs
    // If your RPC returns it wrapped, you might need to unwrap it
    return data.result;
}

/**
 * Analyze a transaction by hash
 * 
 * @param {string} rpcUrl - RPC endpoint URL
 * @param {string} txHash - Transaction hash
 * @returns {Promise<Object>} Analysis result
 */
async function analyzeTransaction(rpcUrl, txHash) {
    await initializeWasm();

    try {
        // Fetch the trace
        console.log(`Fetching trace for ${txHash}...`);
        const trace = await fetchTransactionTrace(rpcUrl, txHash);

        // Analyze the trace
        console.log('Analyzing trace...');
        const traceJson = JSON.stringify(trace);
        const analysisJson = analyze_trace(traceJson);
        const analysis = JSON.parse(analysisJson);

        return {
            success: true,
            txHash,
            analysis,
        };
    } catch (error) {
        console.error('Analysis failed:', error);
        return {
            success: false,
            txHash,
            error: error.message,
        };
    }
}

/**
 * Example: Form handler for gas analysis
 */
function setupGasAnalysisForm() {
    const form = document.getElementById('gas-analysis-form');
    const resultDiv = document.getElementById('result');

    form.addEventListener('submit', async (e) => {
        e.preventDefault();

        const rpcUrl = document.getElementById('rpc-url').value;
        const txHash = document.getElementById('tx-hash').value;

        if (!rpcUrl || !txHash) {
            resultDiv.innerHTML = '<p style="color: red;">Please fill in all fields</p>';
            return;
        }

        resultDiv.innerHTML = '<p>Analyzing transaction...</p>';

        const result = await analyzeTransaction(rpcUrl, txHash);

        if (result.success) {
            const { analysis } = result;
            resultDiv.innerHTML = `
                <h3>Analysis Results</h3>
                <p><strong>Transaction:</strong> ${txHash}</p>
                <p><strong>State Updates:</strong> ${analysis.state_update_count}</p>
                <p><strong>Breakdown:</strong></p>
                <ul>
                    <li>SSTOREs: ${analysis.state_update_breakdown.stores}</li>
                    <li>CALLs: ${analysis.state_update_breakdown.calls}</li>
                    <li>LOG0: ${analysis.state_update_breakdown.log0}</li>
                    <li>LOG1: ${analysis.state_update_breakdown.log1}</li>
                    <li>LOG2: ${analysis.state_update_breakdown.log2}</li>
                    <li>LOG3: ${analysis.state_update_breakdown.log3}</li>
                    <li>LOG4: ${analysis.state_update_breakdown.log4}</li>
                </ul>
                <p><strong>Estimated Gas:</strong> ${analysis.estimated_gas.toLocaleString()}</p>
                <p><strong>Skipped Opcodes:</strong> ${analysis.skipped_opcodes.join(', ') || 'None'}</p>
                <details>
                    <summary>Encoded ABI</summary>
                    <pre style="word-break: break-all;">${analysis.encoded_abi}</pre>
                </details>
            `;
        } else {
            resultDiv.innerHTML = `<p style="color: red;">Error: ${result.error}</p>`;
        }
    });
}

// Export for use in modules
export { initializeWasm, analyzeTransaction, fetchTransactionTrace, setupGasAnalysisForm };

// If running in a script tag, auto-setup
if (typeof window !== 'undefined') {
    window.addEventListener('DOMContentLoaded', () => {
        if (document.getElementById('gas-analysis-form')) {
            setupGasAnalysisForm();
        }
    });
}
