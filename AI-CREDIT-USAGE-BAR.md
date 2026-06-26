# AI Credit Usage Bar

Denne filen beskriver hvordan `ai_credit_status` i Zed kan konfigureres, både lokalt og hos de ulike providerne.

## Lokal konfig i Zed

Legg dette i `settings.json`:

```json
{
  "ai_credit_status": {
    "enabled": true,
    "refresh_seconds": 60,
    "monthly_budget_usd": 100.0
  }
}
```

Felter:

- `enabled`: slår credit-status på/av.
- `refresh_seconds`: hvor ofte data hentes (minimum 15 sekunder).
- `monthly_budget_usd`: valgfritt månedlig budsjett brukt for prosent eller estimat der faktisk usage ikke er tilgjengelig. I estimatmodus vises en grå placeholder-bar.

## Provider-oppsett

## OpenAI

1. Konfigurer OpenAI API key i Zed provider-innstillinger.
2. (Valgfritt/anbefalt for usage) sett `OPENAI_ADMIN_API_KEY` i miljøet for credit-status.
3. Credit-status prøver flere usage/cost-endepunkter automatisk:
   - `GET /v1/organization/costs` (foretrukket)
   - `GET /v1/dashboard/billing/usage`
   - `GET /v1/usage`

### Viktig om tilgang

Noen OpenAI-kontoer returnerer feil om at usage krever browser session key. Da hjelper ofte ikke vanlig API key.

- For programmatisk usage/cost henting trengs ofte **Organization Admin API key**.
- `OPENAI_ADMIN_API_KEY` brukes av credit-status når satt; ellers brukes OpenAI-nøkkelen fra aktiv provider.
- Det finnes vanligvis ikke en enkel konto-toggle som gjør standard API key gyldig for disse endepunktene.

Hvis usage ikke kan hentes:

- med `monthly_budget_usd`: baren viser grå estimatbar (`Usage estimate`)
- uten `monthly_budget_usd`: `"Usage unavailable"`

## OpenRouter

1. Konfigurer `OPENROUTER_API_KEY` eller OpenRouter key i Zed.
2. Credit-status bruker `GET https://openrouter.ai/api/v1/key`.

Hvis `limit` og `limit_remaining` finnes, vises brukt/total + prosent.

## Anthropic (Claude)

Anthropic eksponerer ikke tilsvarende remaining/spend i denne flyten med standard API key.

- Sett `monthly_budget_usd` for grå estimatbar (`Usage estimate`).
- Ellers vises `"Usage unavailable"`.
- Sjekk faktisk forbruk i Anthropic billing:
  `https://console.anthropic.com/settings/billing`

## Mistral

Mistral eksponerer ikke tilsvarende remaining/spend i denne flyten med standard API key.

- Sett `monthly_budget_usd` for grå estimatbar (`Usage estimate`).
- Ellers vises `"Usage unavailable"`.
- Sjekk faktisk forbruk i Mistral billing:
  `https://console.mistral.ai/billing`

## GitHub Copilot

- Logg inn i Copilot/GitHub i Zed.
- Status henter premium quota-data via Copilot-endepunktet.

## Zed Pro

- Brukes når aktiv provider er Zed-hosted modeller.
- Data hentes fra Zed-kontoens token-spend usage.

## Feilsøking

- Hvis status viser `No active AI provider`: velg/sett en aktiv modell i Agent-innstillinger.
- Hvis OpenAI viser session-key-relatert melding: bruk Organization Admin API key for usage/cost API-er, eller bruk `monthly_budget_usd` som estimat.
- Hvis status ikke oppdateres umiddelbart: vent `refresh_seconds` eller restart Zed.