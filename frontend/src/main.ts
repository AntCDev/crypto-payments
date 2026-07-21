import './style.css';
import { generateMnemonic } from '@scure/bip39';
import { wordlist } from '@scure/bip39/wordlists/english.js';

// -----------------------------------------------------------------------------
// Type Definitions matching your Axum Backend
// -----------------------------------------------------------------------------
interface SignUpMerchantRequest {
  name: string;
  slug: string;
  password: string;
  webhook_url?: string;
  mnemonic?: string;
}

interface SignUpMerchantResponse {
  merchant_id: string;
  name: string;
  slug: string;
  status: string;
  api_key_id: string;
  api_key_secret: string;
  mnemonic: string;
  webhook_secret?: string;
}

// -----------------------------------------------------------------------------
// UI Element References
// -----------------------------------------------------------------------------
const form = document.querySelector<HTMLFormElement>('#merchant-form')!;
const nameInput = document.querySelector<HTMLInputElement>('#name')!;
const slugInput = document.querySelector<HTMLInputElement>('#slug')!;
const passwordInput = document.querySelector<HTMLInputElement>('#password')!;
const webhookInput = document.querySelector<HTMLInputElement>('#webhook_url')!;
const mnemonicInput = document.querySelector<HTMLTextAreaElement>('#mnemonic')!;

const genMnemonicBtn = document.querySelector<HTMLButtonElement>('#gen-mnemonic-btn')!;
const submitBtn = document.querySelector<HTMLButtonElement>('#submit-btn')!;
const errorBox = document.querySelector<HTMLDivElement>('#error-box')!;

// Result UI
const credentialsCard = document.querySelector<HTMLDivElement>('#credentials-card')!;
const resMerchantId = document.querySelector<HTMLDivElement>('#res-merchant-id')!;
const resApiKeyId = document.querySelector<HTMLDivElement>('#res-api-key-id')!;
const resApiKeySecret = document.querySelector<HTMLDivElement>('#res-api-key-secret')!;
const resMnemonic = document.querySelector<HTMLDivElement>('#res-mnemonic')!;
const webhookSecretContainer = document.querySelector<HTMLDivElement>('#webhook-secret-container')!;
const resWebhookSecret = document.querySelector<HTMLDivElement>('#res-webhook-secret')!;

// -----------------------------------------------------------------------------
// Event Handlers
// -----------------------------------------------------------------------------

// Auto-fill slug from Merchant Name
nameInput.addEventListener('input', () => {
  slugInput.value = nameInput.value
      .toLowerCase()
      .trim()
      .replace(/[^\w\s-]/g, '')
      .replace(/[\s_-]+/g, '-')
      .replace(/^-+|-+$/g, '');
});

// Client-side Mnemonic Generator
genMnemonicBtn.addEventListener('click', () => {
  mnemonicInput.value = generateMnemonic(wordlist);
});

// Form Submission
form.addEventListener('submit', async (e) => {
  e.preventDefault();

  // Clear previous state
  errorBox.classList.add('hidden');
  errorBox.textContent = '';
  credentialsCard.classList.add('hidden');
  submitBtn.disabled = true;
  submitBtn.textContent = 'Processing...';

  // Construct payload
  const payload: SignUpMerchantRequest = {
    name: nameInput.value.trim(),
    slug: slugInput.value.trim(),
    password: passwordInput.value,
    webhook_url: webhookInput.value.trim() || undefined,
    mnemonic: mnemonicInput.value.trim() || undefined,
  };

  try {
    const response = await fetch('/api/merchants', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });

    if (!response.ok) {
      const errMessage = await response.text();
      throw new Error(errMessage || `Server error ${response.status}`);
    }

    const data: SignUpMerchantResponse = await response.json();

    // Populate response fields
    resMerchantId.textContent = data.merchant_id;
    resApiKeyId.textContent = data.api_key_id;
    resApiKeySecret.textContent = data.api_key_secret;
    resMnemonic.textContent = data.mnemonic;

    if (data.webhook_secret) {
      webhookSecretContainer.classList.remove('hidden');
      resWebhookSecret.textContent = data.webhook_secret;
    } else {
      webhookSecretContainer.classList.add('hidden');
    }

    // Display credentials card and reset form
    credentialsCard.classList.remove('hidden');
    form.reset();

  } catch (err) {
    errorBox.classList.remove('hidden');
    errorBox.textContent = err instanceof Error ? err.message : 'An unknown error occurred.';
  } finally {
    submitBtn.disabled = false;
    submitBtn.textContent = 'Create Merchant Account';
  }
});