# Compliance & Legal Disclaimer

**This document is not legal advice.** It is written by the project author (not a lawyer) to explain, as clearly as possible, why this software's design choices create legal obligations for whoever operates it, so that operators know what questions to bring to their own qualified counsel. Nothing here should be relied on as a substitute for that counsel. Laws referenced here change, and this document may lag behind the current state of the law in any given place — always verify current requirements before acting.

---

## 1. Status of the project

This is an early-stage proof of concept. **Do not deploy it with real funds.** Everything below is written for the person who eventually *does* deploy a production instance, whenever that becomes reasonable — it is here early so the compliance shape is visible from day one, not bolted on later.

## 2. Why this software is custodial, and why that matters

This processor is **fully custodial**. Concretely:

- In naive-QR mode, the software derives and holds the private keys for HD deposit addresses, and later sweeps funds from those addresses to a treasury wallet that the operator's server also controls.
- In WalletConnect mode on EVM, funds are sent to a smart contract that batches payouts — the operator's server holds the admin/owner keys that control that contract.
- In WalletConnect mode on Solana, funds are sent directly to a main account controlled by the operator, tagged with a memo/reference for reconciliation.

In every path, **the operator's server — not the merchant, and not the end customer — controls the keys.** A merchant using this software does not have self-custody, even though the platform may present the funds as "belonging" to that merchant's account. This is the same basic shape as a service like NowPayments or Inqud: you sign up, you're given an address/account, but the platform holds the keys and you withdraw to your own wallet afterward.

**This custodial design is what turns "running some software" into "operating a business that legally looks like a payment/custody service."** Whether that's fine, regulated, or outright restricted depends entirely on where the operator (not the merchant, not the customer) is located and who they're serving. There is no single global answer — this is jurisdiction-by-jurisdiction, and it is the operator's responsibility to check theirs.

## 3. Why the deployment mode matters

### Solo mode (operator = only merchant)

When you are the only merchant, you are custodying your own funds via software you wrote instead of via a personal wallet. This is closer, legally, to ordinary self-custody, and is a meaningfully lower-risk starting point. It is *not* automatically zero-obligation in every jurisdiction — some countries' AML frameworks are triggered by the *activity* of exchanging/holding virtual assets on behalf of "clients or users," and depending on local interpretation, that could still apply even without a third-party merchant. Check locally.

### Multi-tenant mode (open merchant signups)

The moment you allow unrelated third parties to sign up and receive funds through your instance, you are providing a custody/payment service to clients. This is the point at which most jurisdictions' financial-services, money-transmission, virtual-asset-service-provider (VASP), or AML/CTF frameworks are triggered — regardless of whether you call yourself a "processor," a "custodian," or anything else. Regulators generally look at the substance of what you're doing (holding value on behalf of others) rather than the label you use.

**This is why KYB is a hard gate on multi-tenant mode in this project.** Identifying who your merchants are is close to a universal minimum requirement once you're custodying funds for other businesses — even jurisdictions with a light regulatory touch on crypto generally still expect some form of customer/client due diligence from anyone providing this kind of service. The software will not let you enable open merchant signups without a configured KYB provider.

## 4. What this software deliberately does *not* do, and why that doesn't make you exempt from everything

- **No end-customer KYC.** The project does not identify who is sending a payment from a customer wallet. This narrows (but does not eliminate) the compliance surface — you are not building a KYC database of consumers. Depending on your jurisdiction, obligations can still attach to the custody/transmission activity itself, independent of whether you know who the payer was.
- **No fiat on/off-ramp, ever.** Funds never touch a bank account or card network through this software. This removes an entire category of obligations that specifically attach to fiat conversion (e.g. money-transmitter rules that key off "exchanging virtual currency for fiat"). It does **not** remove obligations that attach to custody/exchange of virtual assets between parties, which several jurisdictions regulate independently of any fiat leg.

In short: narrowing scope reduces *some* categories of obligation, but custody of other people's crypto is, on its own, enough to trigger obligations in many places. Don't treat "we don't do KYC or fiat" as a general compliance shortcut — it addresses specific risks, not all of them.

## 5. Worked example: operating from Mexico

This section is included as a concrete illustration of how the above plays out in one specific jurisdiction (the author's own). **It is an example, not a template** — if you are hosting from anywhere else, none of the specific registrations, thresholds, or agency names below apply to you; go find your own jurisdiction's equivalents.

- Mexico does not currently require a specific license from Banxico or the CNBV for a non-financial-institution operator to run a crypto exchange/custody service — that space is a "gray zone": permitted, but not formally licensed, for entities that are not banks or fintech institutions. (Banks and licensed fintech companies are actually *barred* from offering crypto custody/exchange/transmission directly to their own clients — the space an independent operator is allowed to work in is, oddly, more open than the one banks are stuck in.)
- Separately, and regardless of licensing, Mexico's anti-money-laundering law (LFPIORPI) classifies "exchange or commercialization of virtual assets" as a *vulnerable activity* under Article 17. Anyone performing it is a "Sujeto Obligado" and must:
  - Register with the SAT (tax authority) — requiring an RFC and a valid e.firma — before submitting the first notice.
  - File monthly notices (*avisos*) to the UIF (Financial Intelligence Unit) once a client's transactions cross the notice threshold. As of the 2025 reform, that threshold for virtual assets is **210 UMA (~$24,635 MXN)** — a level a moderately active merchant will cross regularly.
  - Maintain identification files (KYB, in this project's case) on clients, verified against official documentation.
  - Identify beneficial owners at a 25% ownership threshold.
  - Retain records for 10 years, and file a notice within 24 hours of detecting anything suspicious.
  - Implement automated detection systems (this is now an explicit statutory requirement, not just good practice).
  - Penalties for failing to file range roughly **$1,173,100–$7,625,150 MXN** — this is not a trivial risk for a side project that quietly crossed into multi-tenant use.
- Mexican fintech/crypto regulation is actively changing. A 2025 reform introduced a formal "Digital Asset Custodian" license concept with capital/audit requirements, and industry groups are pushing for a broader "Fintech Law 2.0" covering crypto licensing more comprehensively as of 2026. **Anyone operating from Mexico should check current status before launching multi-tenant mode, not rely on this document.**
- Collecting merchant KYB documents also means processing personal/business data, which brings in Mexico's data protection law (LFPDPPP) — at minimum, a privacy notice (*aviso de privacidad*) is advisable.
- None of the above is a substitute for tax advice — fees/margin earned as the operator are ordinary taxable income (ISR), independent of any AML question.

## 6. What every operator, in any jurisdiction, should do before enabling multi-tenant mode

1. **Identify your own jurisdiction's equivalent of "vulnerable activity" / AML-obligated-subject status for virtual asset custody or exchange**, and register accordingly if required.
2. **Check whether your jurisdiction requires a money-transmitter, VASP, or custodian license** for holding crypto on behalf of others — even without a fiat leg.
3. **Configure a KYB provider before enabling merchant signups** — this project will not let you skip it, but you still need to actually pick a compliant provider and process for your situation.
4. **Check the jurisdiction of your merchants and their end customers, too** — obligations can attach based on who you're serving, not just where your server sits.
5. **Get an actual lawyer** in your jurisdiction to review your specific setup before opening signups to the public. This document exists to help you ask the right questions, not to answer them for you.

## 7. Scope of this project's responsibility

This is open-source software provided as-is, for educational and portfolio purposes, with no warranty of any kind (see forthcoming `LICENSE` file once finalized). The author is not responsible for how any operator configures, deploys, or represents an instance of this software, including any operator's compliance (or non-compliance) with the laws applicable to them. Each operator is solely responsible for determining and meeting their own legal obligations before accepting real funds from real merchants or customers.
