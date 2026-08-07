import './style.css';
import QRCode from 'qrcode';

// =============================================================================
// EVM checkout page.
//
// Reached as:  /EVM.html?id=3cafe81b-0049-486b-9447-6cc55887bbba
// (the /invoice?id=... handler redirects here once per-chain pages exist)
//
// Two payment paths are always offered:
//   A. naive QR  -> encodes ONLY the deposit address, nothing else. Enriched
//                   payloads (EIP-681 etc.) are handled inconsistently across
//                   wallets, so we deliberately do not use them here.
//   B. wallet    -> connect, verify/raise the ERC20 allowance, then send.
// =============================================================================

// -----------------------------------------------------------------------------
// Backend types (mirror of InvoiceDetailsResponse in the Axum handler)
// -----------------------------------------------------------------------------
interface PaymentSummary {
    amount: string | number;      // rust_decimal -> JSON
    confirmations: number;
    status: string;
}

interface InvoiceDetailsResponse {
    merchant_id: string;
    token_id: string;
    token_address: string | null;
    amount_requested: string | number;
    amount_received: string | number;
    wallet_address: string;
    payment_reference: string;
    status: string;
    created_at: string;
    expires_at: string;
    required_confirmations: number;
    payments: PaymentSummary[];
    wallet_connect_command: string;
}

// -----------------------------------------------------------------------------
// Token / chain metadata
// -----------------------------------------------------------------------------
interface TokenDetails {
    /** ERC20 contract. null / zero-address means the chain's native coin. */
    token_contract_address: string | null;
    /** Contract that pulls the funds — this is the address we approve. */
    payment_contract_address: string | null;
    chain_id: number;
    chain_name: string;
    decimals: number;
    symbol: string;
}

// TODO: replace the hardcoded block below with the real lookup.
//
// async function getTokenDetails(tokenId: string): Promise<TokenDetails> {
//   const res = await fetch(`/api/tokens/${encodeURIComponent(tokenId)}`);
//   if (!res.ok) throw new Error(`GetTokenDetails failed: ${res.status}`);
//   const json = await res.json();
//   return {
//     token_contract_address: json.token_contract_address,
//     payment_contract_address: json.payment_contract_address,
//     chain_id: json.chain_id,
//     chain_name: json.chain_name,
//     decimals: json.decimals,
//     symbol: json.symbol,
//   };
// }

/** TEMPORARY — fill these in for testing. Sepolia USDC as a placeholder. */
const HARDCODED_TOKEN_DETAILS: TokenDetails = {
    token_contract_address: '0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238',
    payment_contract_address: '0x0000000000000000000000000000000000000000', // <- your payment contract
    chain_id: 11155111,
    chain_name: 'Sepolia',
    decimals: 6,
    symbol: 'USDC',
};

/**
 * Is `amount_requested` already in base units (wei / smallest unit)?
 * true  -> "1500000" with 6 decimals renders as 1.5
 * false -> "1.5" is a human amount and gets scaled up for the transaction
 * Flip this one flag if the backend changes its mind.
 */
const AMOUNT_IS_BASE_UNITS = true;

/** How often to re-poll the invoice for on-chain updates, in ms. */
const POLL_INTERVAL_MS = 15_000;

const ZERO_ADDRESS = '0x0000000000000000000000000000000000000000';

// -----------------------------------------------------------------------------
// Minimal EIP-1193 provider surface
// -----------------------------------------------------------------------------
interface Eip1193Provider {
    request(args: { method: string; params?: unknown[] | object }): Promise<any>;
    on?(event: string, handler: (...args: any[]) => void): void;
}

declare global {
    interface Window {
        ethereum?: Eip1193Provider;
    }
}

// -----------------------------------------------------------------------------
// DOM
// -----------------------------------------------------------------------------
const $ = <T extends HTMLElement>(sel: string): T => document.querySelector<T>(sel)!;

const loadingState = $<HTMLDivElement>('#loading-state');
const invoiceView = $<HTMLElement>('#invoice-view');
const errorBox = $<HTMLDivElement>('#error-box');
const noticeBox = $<HTMLDivElement>('#notice-box');

const hdrInvoiceId = $<HTMLSpanElement>('#hdr-invoice-id');
const hdrStatus = $<HTMLSpanElement>('#hdr-status');
const hdrExpiry = $<HTMLParagraphElement>('#hdr-expiry');

const amountDue = $<HTMLSpanElement>('#amount-due');
const amountSymbol = $<HTMLSpanElement>('#amount-symbol');
const amountBaseUnits = $<HTMLParagraphElement>('#amount-base-units');
const amountReceived = $<HTMLSpanElement>('#amount-received');
const chainLabel = $<HTMLSpanElement>('#chain-label');
const progressBar = $<HTMLDivElement>('#progress-bar');
const progressLabel = $<HTMLParagraphElement>('#progress-label');

const addressQr = $<HTMLCanvasElement>('#address-qr');
const walletAddressEl = $<HTMLDivElement>('#wallet-address');
const copyAddressBtn = $<HTMLButtonElement>('#copy-address-btn');
const referenceBlock = $<HTMLDivElement>('#reference-block');
const paymentReferenceEl = $<HTMLDivElement>('#payment-reference');
const warnSymbol = $<HTMLSpanElement>('#warn-symbol');
const warnChain = $<HTMLSpanElement>('#warn-chain');

const btnConnect = $<HTMLButtonElement>('#btn-connect');
const btnApprove = $<HTMLButtonElement>('#btn-approve');
const btnSend = $<HTMLButtonElement>('#btn-send');
const connectDetail = $<HTMLParagraphElement>('#connect-detail');
const approveDetail = $<HTMLParagraphElement>('#approve-detail');
const sendDetail = $<HTMLParagraphElement>('#send-detail');
const stepMarkers = [$<HTMLSpanElement>('#step-1-marker'), $<HTMLSpanElement>('#step-2-marker'), $<HTMLSpanElement>('#step-3-marker')];
const walletLog = $<HTMLDivElement>('#wallet-log');

const paymentsSection = $<HTMLElement>('#payments-section');
const paymentsList = $<HTMLDivElement>('#payments-list');

// -----------------------------------------------------------------------------
// Page state
// -----------------------------------------------------------------------------
let invoiceId = '';
let invoice: InvoiceDetailsResponse | null = null;
let tokenDetails: TokenDetails = HARDCODED_TOKEN_DETAILS;

let provider: Eip1193Provider | null = null;
let account: string | null = null;
let amountBase = 0n;          // amount due, in base units
let isErc20 = false;
let spender = ZERO_ADDRESS;   // contract we approve / send to
let tokenContract = ZERO_ADDRESS;

// =============================================================================
// Fixed-point helpers — all amounts stay in BigInt, never in float
// =============================================================================
function parseUnits(value: string, decimals: number): bigint {
    const trimmed = value.trim();
    const negative = trimmed.startsWith('-');
    const [whole, frac = ''] = trimmed.replace(/^[-+]/, '').split('.');
    const padded = (frac + '0'.repeat(decimals)).slice(0, decimals);
    const raw = BigInt((whole || '0') + (decimals > 0 ? padded : ''));
    return negative ? -raw : raw;
}

function formatUnits(value: bigint, decimals: number): string {
    const negative = value < 0n;
    const abs = negative ? -value : value;
    const s = abs.toString().padStart(decimals + 1, '0');
    const whole = s.slice(0, s.length - decimals) || '0';
    const frac = decimals > 0 ? s.slice(s.length - decimals).replace(/0+$/, '') : '';
    const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ',');
    return `${negative ? '-' : ''}${grouped}${frac ? '.' + frac : ''}`;
}

/** Normalise whatever the backend sent into base units. */
function toBaseUnits(value: string | number, decimals: number): bigint {
    const asString = typeof value === 'number' ? value.toString() : value;
    if (AMOUNT_IS_BASE_UNITS) {
        // Already smallest-unit; tolerate a trailing ".0" from the Decimal type.
        return BigInt(asString.trim().split('.')[0] || '0');
    }
    return parseUnits(asString, decimals);
}

// =============================================================================
// ABI encoding — hand-rolled so the page carries no web3 library
// =============================================================================
const SELECTOR = {
    allowance: '0xdd62ed3e', // allowance(address,address)
    approve: '0x095ea7b3',   // approve(address,uint256)
    balanceOf: '0x70a08231', // balanceOf(address)
    decimals: '0x313ce567',  // decimals()
    transfer: '0xa9059cbb',  // transfer(address,uint256)
} as const;

const padAddress = (addr: string): string => addr.toLowerCase().replace(/^0x/, '').padStart(64, '0');
const padUint = (n: bigint): string => n.toString(16).padStart(64, '0');
// const MAX_UINT256 = (1n << 256n) - 1n;

function isNullAddress(addr: string | null | undefined): boolean {
    if (!addr) return true;
    const a = addr.trim().toLowerCase();
    return a === '' || a === '0' || a === '0x' || a === 'null' || a === ZERO_ADDRESS;
}

function shortAddress(addr: string): string {
    return `${addr.slice(0, 6)}…${addr.slice(-4)}`;
}

// =============================================================================
// UI helpers
// =============================================================================
function showError(message: string): void {
    errorBox.textContent = message;
    errorBox.classList.remove('hidden');
}

function clearError(): void {
    errorBox.classList.add('hidden');
    errorBox.textContent = '';
}

function showNotice(message: string): void {
    noticeBox.textContent = message;
    noticeBox.classList.remove('hidden');
}

function log(message: string): void {
    walletLog.classList.remove('hidden');
    const line = document.createElement('div');
    line.textContent = `> ${message}`;
    walletLog.appendChild(line);
    walletLog.scrollTop = walletLog.scrollHeight;
}

/** 0 = nothing done, 1 = connected, 2 = approved, 3 = sent. */
function setStep(step: 0 | 1 | 2 | 3): void {
    stepMarkers.forEach((marker, i) => {
        const done = i < step;
        const active = i === step;
        marker.className =
            'mt-0.5 shrink-0 w-6 h-6 rounded-full text-xs font-bold flex items-center justify-center border ' +
            (done
                ? 'bg-emerald-500 border-emerald-500 text-slate-950'
                : active
                    ? 'border-emerald-500/60 text-emerald-400'
                    : 'border-slate-700 text-slate-600');
        marker.textContent = done ? '✓' : String(i + 1);
    });

    btnConnect.disabled = step >= 1;
    btnApprove.disabled = step !== 1;
    btnSend.disabled = step !== 2;

    if (step >= 1) btnConnect.textContent = 'Wallet connected';
}

function setBusy(btn: HTMLButtonElement, busy: boolean, busyLabel: string, idleLabel: string): void {
    btn.disabled = busy;
    btn.textContent = busy ? busyLabel : idleLabel;
}

function statusClasses(status: string): string {
    const base = 'inline-block px-3 py-1 rounded-full text-xs font-bold uppercase tracking-wider border ';
    switch (status.toLowerCase()) {
        case 'paid':
        case 'confirmed':
        case 'completed':
            return base + 'bg-emerald-500/10 border-emerald-500/30 text-emerald-400';
        case 'expired':
        case 'failed':
        case 'cancelled':
            return base + 'bg-rose-500/10 border-rose-500/30 text-rose-400';
        case 'partial':
        case 'pending':
        case 'confirming':
            return base + 'bg-amber-500/10 border-amber-500/30 text-amber-300';
        default:
            return base + 'bg-slate-800 border-slate-700 text-slate-400';
    }
}

// =============================================================================
// Invoice loading & rendering
// =============================================================================
function readInvoiceIdFromUrl(): string {
    const params = new URLSearchParams(window.location.search);
    const fromQuery = params.get('id');
    if (fromQuery) return fromQuery;

    // Fallback: /invoice/<uuid> style paths.
    const fromPath = window.location.pathname.match(
        /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i,
    );
    return fromPath ? fromPath[0] : '';
}

async function fetchInvoice(id: string): Promise<InvoiceDetailsResponse> {
    const res = await fetch(`/api/invoices/${encodeURIComponent(id)}`);
    if (!res.ok) {
        const body = await res.text();
        throw new Error(body || `Could not load invoice (${res.status})`);
    }
    return res.json();
}

function renderInvoice(data: InvoiceDetailsResponse): void {
    invoice = data;

    const { decimals, symbol, chain_name, chain_id } = tokenDetails;

    amountBase = toBaseUnits(data.amount_requested, decimals);
    const receivedBase = toBaseUnits(data.amount_received, decimals);

    // ---- header ----
    hdrInvoiceId.textContent = invoiceId;
    hdrStatus.textContent = data.status;
    hdrStatus.className = statusClasses(data.status);

    // ---- amounts ----
    amountDue.textContent = formatUnits(amountBase, decimals);
    amountSymbol.textContent = symbol;
    amountBaseUnits.textContent = `${amountBase.toString()} base units · ${decimals} decimals`;
    amountReceived.textContent = `${formatUnits(receivedBase, decimals)} ${symbol}`;
    chainLabel.textContent = `${chain_name} (${chain_id})`;

    const pct = amountBase > 0n ? Number((receivedBase * 10_000n) / amountBase) / 100 : 0;
    progressBar.style.width = `${Math.min(pct, 100)}%`;
    progressLabel.textContent =
        receivedBase >= amountBase
            ? `Fully funded · waiting for ${data.required_confirmations} confirmations`
            : `${pct.toFixed(1)}% received · ${formatUnits(amountBase - receivedBase, decimals)} ${symbol} outstanding`;

    // ---- naive QR: address only ----
    walletAddressEl.textContent = data.wallet_address;
    QRCode.toCanvas(addressQr, data.wallet_address, {
        width: 280,
        margin: 1,
        errorCorrectionLevel: 'M',
        color: { dark: '#020617', light: '#ffffff' },
    }).catch((err: unknown) => {
        showError(`The QR code could not be drawn: ${err instanceof Error ? err.message : String(err)}`);
    });

    if (data.payment_reference) {
        referenceBlock.classList.remove('hidden');
        paymentReferenceEl.textContent = data.payment_reference;
    }

    warnSymbol.textContent = symbol;
    warnChain.textContent = chain_name;

    // ---- token routing ----
    tokenContract = !isNullAddress(tokenDetails.token_contract_address)
        ? tokenDetails.token_contract_address!
        : (data.token_address ?? ZERO_ADDRESS);
    isErc20 = !isNullAddress(tokenContract);
    spender = !isNullAddress(tokenDetails.payment_contract_address)
        ? tokenDetails.payment_contract_address!
        : data.wallet_address;

    btnApprove.textContent = isErc20 ? 'Check spending approval' : 'Check balance';

    // ---- expiry ----
    startExpiryCountdown(new Date(data.expires_at));

    // ---- payments ----
    renderPayments(data.payments, decimals, symbol);

    loadingState.classList.add('hidden');
    invoiceView.classList.remove('hidden');

    // Terminal states: nothing left to pay.
    const terminal = ['paid', 'confirmed', 'completed', 'expired', 'cancelled'];
    if (terminal.includes(data.status.toLowerCase())) {
        [btnConnect, btnApprove, btnSend].forEach((b) => (b.disabled = true));
        showNotice(`This invoice is ${data.status.toLowerCase()}. No further payment is needed.`);
    }
}

function renderPayments(payments: PaymentSummary[], decimals: number, symbol: string): void {
    if (!payments.length) {
        paymentsSection.classList.add('hidden');
        return;
    }
    paymentsSection.classList.remove('hidden');
    paymentsList.innerHTML = '';

    for (const p of payments) {
        const row = document.createElement('div');
        row.className =
            'flex items-center justify-between gap-4 p-3 bg-slate-900 rounded-lg border border-slate-800 text-xs';
        row.innerHTML = `
      <span class="font-mono font-bold text-slate-200 tabular-nums">
        ${formatUnits(toBaseUnits(p.amount, decimals), decimals)} ${symbol}
      </span>
      <span class="text-slate-400">${p.confirmations} confirmations</span>
      <span class="${statusClasses(p.status)}">${p.status}</span>`;
        paymentsList.appendChild(row);
    }
}

let expiryTimer: number | undefined;
function startExpiryCountdown(expiresAt: Date): void {
    if (expiryTimer) window.clearInterval(expiryTimer);

    const tick = () => {
        const msLeft = expiresAt.getTime() - Date.now();
        if (msLeft <= 0) {
            hdrExpiry.textContent = 'Expired';
            hdrExpiry.className = 'text-xs text-rose-400 mt-2 font-mono';
            window.clearInterval(expiryTimer);
            return;
        }
        const totalSeconds = Math.floor(msLeft / 1000);
        const h = String(Math.floor(totalSeconds / 3600)).padStart(2, '0');
        const m = String(Math.floor((totalSeconds % 3600) / 60)).padStart(2, '0');
        const s = String(totalSeconds % 60).padStart(2, '0');
        hdrExpiry.textContent = `Expires in ${h}:${m}:${s}`;
    };

    tick();
    expiryTimer = window.setInterval(tick, 1000);
}

// =============================================================================
// Wallet: step 1 — connect
// =============================================================================
function getProvider(): Eip1193Provider {
    // Injected wallet (MetaMask, Rabby, Coinbase, …). Every EIP-1193 provider
    // exposes the same request() surface.
    if (window.ethereum) return window.ethereum;

    // TODO: swap in WalletConnect for wallets that aren't injected. Because it is
    // also EIP-1193, nothing below this function has to change:
    //
    //   import { EthereumProvider } from '@walletconnect/ethereum-provider';
    //   const wc = await EthereumProvider.init({
    //     projectId: import.meta.env.VITE_WC_PROJECT_ID,
    //     chains: [tokenDetails.chain_id],
    //     showQrModal: true,
    //   });
    //   await wc.connect();
    //   return wc;

    throw new Error('No wallet found in this browser. Use the address QR instead, or open this page inside your wallet.');
}

async function ensureChain(p: Eip1193Provider, chainId: number): Promise<void> {
    const current = (await p.request({ method: 'eth_chainId' })) as string;
    if (parseInt(current, 16) === chainId) return;

    log(`switching to chain ${chainId}…`);
    try {
        await p.request({
            method: 'wallet_switchEthereumChain',
            params: [{ chainId: '0x' + chainId.toString(16) }],
        });
    } catch (err: any) {
        // 4902 = chain unknown to the wallet.
        if (err?.code === 4902) {
            // TODO: add rpc_url / explorer / native currency to GetTokenDetails and
            // uncomment to let the wallet add the chain itself.
            //
            // await p.request({
            //   method: 'wallet_addEthereumChain',
            //   params: [{
            //     chainId: '0x' + chainId.toString(16),
            //     chainName: tokenDetails.chain_name,
            //     rpcUrls: [tokenDetails.rpc_url],
            //     nativeCurrency: { name: 'ETH', symbol: 'ETH', decimals: 18 },
            //     blockExplorerUrls: [tokenDetails.block_explorer],
            //   }],
            // });
            throw new Error(`Add ${tokenDetails.chain_name} to your wallet, then connect again.`);
        }
        throw err;
    }
}

btnConnect.addEventListener('click', async () => {
    clearError();
    setBusy(btnConnect, true, 'Waiting for wallet…', 'Connect wallet');

    try {
        provider = getProvider();
        const accounts = (await provider.request({ method: 'eth_requestAccounts' })) as string[];
        if (!accounts?.length) throw new Error('Your wallet returned no accounts.');

        account = accounts[0];
        await ensureChain(provider, tokenDetails.chain_id);

        connectDetail.textContent = `${shortAddress(account)} on ${tokenDetails.chain_name}`;
        log(`connected ${account}`);
        setStep(1);

        // Re-run the flow if the user swaps account or chain mid-checkout.
        provider.on?.('accountsChanged', () => window.location.reload());
        provider.on?.('chainChanged', () => window.location.reload());
    } catch (err) {
        showError(err instanceof Error ? err.message : 'The wallet could not be connected.');
        setBusy(btnConnect, false, '', 'Connect wallet');
        setStep(0);
    }
});

// =============================================================================
// Wallet: step 2 — allowance check, and approval if it falls short
// =============================================================================
async function readAllowance(p: Eip1193Provider, owner: string): Promise<bigint> {
    const data = SELECTOR.allowance + padAddress(owner) + padAddress(spender);
    const result = (await p.request({
        method: 'eth_call',
        params: [{ to: tokenContract, data }, 'latest'],
    })) as string;
    return BigInt(result === '0x' ? '0x0' : result);
}

async function readErc20Balance(p: Eip1193Provider, owner: string): Promise<bigint> {
    const data = SELECTOR.balanceOf + padAddress(owner);
    const result = (await p.request({
        method: 'eth_call',
        params: [{ to: tokenContract, data }, 'latest'],
    })) as string;
    return BigInt(result === '0x' ? '0x0' : result);
}

async function readNativeBalance(p: Eip1193Provider, owner: string): Promise<bigint> {
    const result = (await p.request({ method: 'eth_getBalance', params: [owner, 'latest'] })) as string;
    return BigInt(result);
}

async function sendApproval(p: Eip1193Provider, owner: string, amount: bigint): Promise<string> {
    const data = SELECTOR.approve + padAddress(spender) + padUint(amount);
    return (await p.request({
        method: 'eth_sendTransaction',
        params: [{ from: owner, to: tokenContract, data }],
    })) as string;
}

async function waitForReceipt(p: Eip1193Provider, txHash: string, timeoutMs = 180_000): Promise<any> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
        const receipt = await p.request({ method: 'eth_getTransactionReceipt', params: [txHash] });
        if (receipt) return receipt;
        await new Promise((r) => setTimeout(r, 3000));
    }
    throw new Error('The transaction is taking longer than expected. Check your wallet, then reload this page.');
}

btnApprove.addEventListener('click', async () => {
    if (!provider || !account) return;
    clearError();
    const idleLabel = isErc20 ? 'Check spending approval' : 'Check balance';
    setBusy(btnApprove, true, 'Checking…', idleLabel);

    try {
        // ---- native coin: no allowance concept, just make sure the balance covers it ----
        if (!isErc20) {
            log('native transfer — no approval needed');
            const balance = await readNativeBalance(provider, account);
            if (balance < amountBase) {
                throw new Error(
                    `Your balance is ${formatUnits(balance, tokenDetails.decimals)} ${tokenDetails.symbol}, ` +
                    `which is short of the ${formatUnits(amountBase, tokenDetails.decimals)} ${tokenDetails.symbol} due.`,
                );
            }
            approveDetail.textContent = `Balance ${formatUnits(balance, tokenDetails.decimals)} ${tokenDetails.symbol} — ready to send`;
            setStep(2);
            return;
        }

        // ---- ERC20 ----
        log(`token ${shortAddress(tokenContract)} is an ERC20 — checking allowance for ${shortAddress(spender)}`);

        const balance = await readErc20Balance(provider, account);
        if (balance < amountBase) {
            throw new Error(
                `Your balance is ${formatUnits(balance, tokenDetails.decimals)} ${tokenDetails.symbol}, ` +
                `which is short of the ${formatUnits(amountBase, tokenDetails.decimals)} ${tokenDetails.symbol} due.`,
            );
        }

        let allowance = await readAllowance(provider, account);
        log(`allowance = ${formatUnits(allowance, tokenDetails.decimals)} ${tokenDetails.symbol}`);

        if (allowance < amountBase) {
            // Short — ask for exactly the amount due. Swap `amountBase` for
            // MAX_UINT256 if you'd rather approve once and reuse it.
            approveDetail.textContent = 'Approval needed — confirm in your wallet';
            setBusy(btnApprove, true, 'Approve in wallet…', idleLabel);

            // Some tokens (USDT and friends) reject a non-zero -> non-zero approval,
            // so reset to zero first when there is a stale allowance.
            if (allowance > 0n) {
                log('resetting stale allowance to 0 first');
                const resetHash = await sendApproval(provider, account, 0n);
                await waitForReceipt(provider, resetHash);
            }

            const txHash = await sendApproval(provider, account, amountBase);
            log(`approval sent: ${txHash}`);
            setBusy(btnApprove, true, 'Waiting for confirmation…', idleLabel);

            const receipt = await waitForReceipt(provider, txHash);
            if (receipt.status && BigInt(receipt.status) !== 1n) {
                throw new Error('The approval transaction reverted. Try again from your wallet.');
            }

            allowance = await readAllowance(provider, account);
            if (allowance < amountBase) {
                throw new Error('The approval went through but the allowance is still too low. Try approving again.');
            }
        }

        approveDetail.textContent = `Approved ${formatUnits(allowance, tokenDetails.decimals)} ${tokenDetails.symbol} for ${shortAddress(spender)}`;
        log('allowance is sufficient');
        setStep(2);
    } catch (err: any) {
        // 4001 = user rejected in the wallet.
        const message =
            err?.code === 4001
                ? 'You dismissed the request in your wallet. Press the button again when you are ready.'
                : err instanceof Error
                    ? err.message
                    : 'The approval could not be checked.';
        showError(message);
        setStep(1);
    } finally {
        btnApprove.textContent = idleLabel;
    }
});

// =============================================================================
// Wallet: step 3 — send the payment
// =============================================================================
type ParsedCommand =
    | { kind: 'dummy' }
    | { kind: 'tx'; to: string; data?: string; value?: string };

/**
 * Turn `wallet_connect_command` into transaction parameters.
 * Accepts a JSON tx object or an EIP-681 URI; "DUMMY_COMMAND" is recognised as
 * the backend's placeholder and reported as such.
 */
function parseWalletConnectCommand(raw: string): ParsedCommand {
    const command = (raw || '').trim();
    if (!command || command === 'DUMMY_COMMAND') return { kind: 'dummy' };

    // JSON: {"to":"0x…","data":"0x…","value":"0x0"}
    if (command.startsWith('{')) {
        const parsed = JSON.parse(command);
        if (!parsed.to) throw new Error('The payment command from the server has no destination address.');
        return { kind: 'tx', to: parsed.to, data: parsed.data, value: parsed.value };
    }

    // EIP-681: ethereum:0xTarget@chainId/transfer?address=0x…&uint256=…
    if (command.startsWith('ethereum:')) {
        const [target, query = ''] = command.slice('ethereum:'.length).split('?');
        const [addressPart, functionName] = target.split('/');
        const to = addressPart.split('@')[0];
        const params = new URLSearchParams(query);

        if (functionName === 'transfer') {
            const recipient = params.get('address') ?? '';
            const value = params.get('uint256') ?? '0';
            return { kind: 'tx', to, data: SELECTOR.transfer + padAddress(recipient) + padUint(BigInt(value)) };
        }
        return { kind: 'tx', to, value: '0x' + BigInt(params.get('value') ?? '0').toString(16) };
    }

    throw new Error('The payment command from the server is in a format this page does not understand.');
}

/** Fallback used while the backend command is still a placeholder. */
function buildLocalTransaction(): { to: string; data?: string; value?: string } {
    if (!invoice) throw new Error('The invoice is not loaded.');

    if (isErc20) {
        // Straight ERC20 transfer to the deposit address. If you route through a
        // payment contract instead, encode that call here — the allowance checked
        // in step 2 is already granted to `spender`.
        return {
            to: tokenContract,
            data: SELECTOR.transfer + padAddress(invoice.wallet_address) + padUint(amountBase),
        };
    }
    return { to: invoice.wallet_address, value: '0x' + amountBase.toString(16) };
}

btnSend.addEventListener('click', async () => {
    if (!provider || !account || !invoice) return;
    clearError();
    setBusy(btnSend, true, 'Preparing…', 'Send payment');

    try {
        const command = parseWalletConnectCommand(invoice.wallet_connect_command);

        const tx =
            command.kind === 'tx'
                ? { from: account, to: command.to, data: command.data, value: command.value }
                : { from: account, ...buildLocalTransaction() };

        if (command.kind === 'dummy') {
            log('server sent DUMMY_COMMAND — falling back to a locally built transaction');
        }

        log(`prepared tx -> to ${shortAddress(tx.to)}${tx.value ? `, value ${tx.value}` : ''}`);
        log(`data ${tx.data ? tx.data.slice(0, 42) + '…' : '(none)'}`);

        // -------------------------------------------------------------------------
        // The send itself is held back until the backend returns a real
        // wallet_connect_command. To go live: implement the command, then delete
        // the `showNotice` line below and uncomment this block. Nothing else in
        // this file has to change — `tx` is already in eth_sendTransaction shape.
        // -------------------------------------------------------------------------
        //
        // const txHash = (await provider.request({
        //   method: 'eth_sendTransaction',
        //   params: [tx],
        // })) as string;
        //
        // log(`payment sent: ${txHash}`);
        // sendDetail.textContent = txHash;
        // setBusy(btnSend, true, 'Waiting for confirmation…', 'Send payment');
        //
        // const receipt = await waitForReceipt(provider, txHash);
        // if (receipt.status && BigInt(receipt.status) !== 1n) {
        //   throw new Error('The payment transaction reverted. Nothing was transferred.');
        // }
        //
        // setStep(3);
        // btnSend.textContent = 'Payment sent';
        // showNotice('Payment sent. This page will update once the network confirms it.');
        // await refreshInvoice();
        // return;

        showNotice(
            'Sending is switched off until the server returns a real payment command. ' +
            'The transaction that would be sent is printed in the log below — use the address QR to pay in the meantime.',
        );
        sendDetail.textContent = 'Sending disabled — see the log';
        setBusy(btnSend, false, '', 'Send payment');
    } catch (err: any) {
        const message =
            err?.code === 4001
                ? 'You dismissed the payment in your wallet. Press the button again when you are ready.'
                : err instanceof Error
                    ? err.message
                    : 'The payment could not be sent.';
        showError(message);
        setBusy(btnSend, false, '', 'Send payment');
    }
});

// =============================================================================
// Copy button
// =============================================================================
copyAddressBtn.addEventListener('click', async () => {
    if (!invoice) return;
    try {
        await navigator.clipboard.writeText(invoice.wallet_address);
        copyAddressBtn.textContent = 'Copied';
        window.setTimeout(() => (copyAddressBtn.textContent = 'Copy'), 1500);
    } catch {
        showError('Copying failed. Select the address and copy it manually.');
    }
});

// =============================================================================
// Boot
// =============================================================================
async function refreshInvoice(): Promise<void> {
    if (!invoiceId) return;
    try {
        const data = await fetchInvoice(invoiceId);
        renderInvoice(data);
    } catch {
        // Stay quiet on background refresh failures; the page still shows the last
        // good state and the next poll will try again.
    }
}

async function init(): Promise<void> {
    invoiceId = readInvoiceIdFromUrl();

    if (!invoiceId) {
        loadingState.classList.add('hidden');
        showError('This link is missing an invoice id. Open the payment link your merchant sent you.');
        return;
    }

    try {
        const data = await fetchInvoice(invoiceId);

        // TODO: replace with the real lookup once GetTokenDetails exists —
        //   tokenDetails = await getTokenDetails(data.token_id);
        tokenDetails = HARDCODED_TOKEN_DETAILS;

        renderInvoice(data);
        setStep(0);

        window.setInterval(refreshInvoice, POLL_INTERVAL_MS);
    } catch (err) {
        loadingState.classList.add('hidden');
        showError(err instanceof Error ? err.message : 'The invoice could not be loaded.');
    }
}

void init();