use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Burn, Mint, MintTo, Token, TokenAccount};

declare_id!("EHqmmpLnfmcG74SYBCYiA371PmtdpkvbfSAQc7uTRYe9");

// Token has 6 decimals. 1,073,000,000 tokens sit in the virtual reserve and
// 793,100,000 of them can be sold along the curve (pump.fun style constant product).
pub const DECIMALS: u8 = 6;
pub const VIRTUAL_TOKENS: u64 = 1_073_000_000_000_000;
pub const SALE_SUPPLY: u64 = 793_100_000_000_000;
pub const VIRTUAL_SOL: u64 = 2_000_000_000; // 2 SOL
pub const FEE_BPS: u64 = 100; // 1% of every trade goes to the creator

#[program]
pub mod bonding_curve {
    use super::*;

    pub fn launch(ctx: Context<Launch>, name: String, symbol: String, uri: String) -> Result<()> {
        require!(name.len() <= 32, CurveError::TextTooLong);
        require!(symbol.len() <= 10, CurveError::TextTooLong);
        require!(uri.len() <= 128, CurveError::TextTooLong);
        let c = &mut ctx.accounts.curve;
        c.creator = ctx.accounts.creator.key();
        c.mint = ctx.accounts.mint.key();
        c.name = name;
        c.symbol = symbol;
        c.uri = uri;
        c.virtual_sol = VIRTUAL_SOL;
        c.virtual_tokens = VIRTUAL_TOKENS;
        c.real_sol = 0;
        c.tokens_sold = 0;
        c.complete = false;
        c.bump = ctx.bumps.curve;
        Ok(())
    }

    /// Spend up to `max_sol` lamports (fee included) for at least `min_tokens` base units.
    pub fn buy(ctx: Context<Trade>, max_sol: u64, min_tokens: u64) -> Result<()> {
        let c = &mut ctx.accounts.curve;
        require!(!c.complete, CurveError::CurveComplete);
        require!(max_sol > 0, CurveError::ZeroAmount);

        let k = (c.virtual_sol as u128) * (c.virtual_tokens as u128);
        let mut net = (max_sol as u128) * (10_000 - FEE_BPS as u128) / 10_000;
        let remaining = SALE_SUPPLY - c.tokens_sold;

        let new_vs = (c.virtual_sol as u128) + net;
        let mut new_vt = ceil_div(k, new_vs);
        let mut out = (c.virtual_tokens as u128).saturating_sub(new_vt);
        if out >= remaining as u128 {
            // last buy: take only what is left and charge only what that costs
            out = remaining as u128;
            new_vt = (c.virtual_tokens as u128) - out;
            let need_vs = ceil_div(k, new_vt);
            net = need_vs - (c.virtual_sol as u128);
            c.complete = true;
        }
        let out = out as u64;
        require!(out > 0 && out >= min_tokens, CurveError::Slippage);
        let net = net as u64;
        let fee = ceil_div((net as u128) * FEE_BPS as u128, 10_000 - FEE_BPS as u128) as u64;
        require!(net + fee <= max_sol, CurveError::Slippage);

        c.virtual_sol += net;
        c.virtual_tokens = new_vt as u64;
        c.real_sol += net;
        c.tokens_sold += out;

        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.user.to_account_info(),
                    to: c.to_account_info(),
                },
            ),
            net,
        )?;
        if fee > 0 {
            system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.key(),
                    system_program::Transfer {
                        from: ctx.accounts.user.to_account_info(),
                        to: ctx.accounts.creator.to_account_info(),
                    },
                ),
                fee,
            )?;
        }

        let mint_key = c.mint;
        let seeds: &[&[u8]] = &[b"curve", mint_key.as_ref(), &[c.bump]];
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                MintTo {
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.user_token.to_account_info(),
                    authority: c.to_account_info(),
                },
                &[seeds],
            ),
            out,
        )?;

        emit!(TradeEvent {
            mint: mint_key,
            user: ctx.accounts.user.key(),
            is_buy: true,
            sol: net + fee,
            tokens: out,
            virtual_sol: c.virtual_sol,
            virtual_tokens: c.virtual_tokens,
            ts: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Sell `tokens` base units for at least `min_sol` lamports (after the fee).
    pub fn sell(ctx: Context<Trade>, tokens: u64, min_sol: u64) -> Result<()> {
        let c = &mut ctx.accounts.curve;
        require!(tokens > 0, CurveError::ZeroAmount);
        require!(tokens <= c.tokens_sold, CurveError::ZeroAmount);

        let k = (c.virtual_sol as u128) * (c.virtual_tokens as u128);
        let new_vt = (c.virtual_tokens as u128) + tokens as u128;
        let new_vs = ceil_div(k, new_vt);
        let gross = ((c.virtual_sol as u128) - new_vs) as u64;
        require!(gross <= c.real_sol, CurveError::Slippage);
        let fee = (gross as u128 * FEE_BPS as u128 / 10_000) as u64;
        let payout = gross - fee;
        require!(payout >= min_sol && payout > 0, CurveError::Slippage);

        c.virtual_sol = new_vs as u64;
        c.virtual_tokens = new_vt as u64;
        c.real_sol -= gross;
        c.tokens_sold -= tokens;
        c.complete = false;

        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.user_token.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            tokens,
        )?;

        // the curve account is owned by this program, so it can pay out directly
        let curve_info = c.to_account_info();
        **curve_info.try_borrow_mut_lamports()? -= gross;
        **ctx.accounts.user.to_account_info().try_borrow_mut_lamports()? += payout;
        **ctx.accounts.creator.to_account_info().try_borrow_mut_lamports()? += fee;

        emit!(TradeEvent {
            mint: c.mint,
            user: ctx.accounts.user.key(),
            is_buy: false,
            sol: payout,
            tokens,
            virtual_sol: c.virtual_sol,
            virtual_tokens: c.virtual_tokens,
            ts: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }
}

fn ceil_div(a: u128, b: u128) -> u128 {
    (a + b - 1) / b
}

#[derive(Accounts)]
pub struct Launch<'info> {
    #[account(
        init,
        payer = creator,
        space = 8 + Curve::INIT_SPACE,
        seeds = [b"curve", mint.key().as_ref()],
        bump
    )]
    pub curve: Account<'info, Curve>,
    #[account(
        init,
        payer = creator,
        mint::decimals = DECIMALS,
        mint::authority = curve,
    )]
    pub mint: Account<'info, Mint>,
    #[account(mut)]
    pub creator: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Trade<'info> {
    #[account(mut, seeds = [b"curve", mint.key().as_ref()], bump = curve.bump, has_one = mint, has_one = creator)]
    pub curve: Account<'info, Curve>,
    #[account(mut)]
    pub mint: Account<'info, Mint>,
    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = mint,
        associated_token::authority = user,
    )]
    pub user_token: Account<'info, TokenAccount>,
    /// CHECK: checked against curve.creator by has_one; receives the 1% fee
    #[account(mut)]
    pub creator: UncheckedAccount<'info>,
    #[account(mut)]
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[account]
#[derive(InitSpace)]
pub struct Curve {
    pub creator: Pubkey,
    pub mint: Pubkey,
    #[max_len(32)]
    pub name: String,
    #[max_len(10)]
    pub symbol: String,
    #[max_len(128)]
    pub uri: String,
    pub virtual_sol: u64,
    pub virtual_tokens: u64,
    pub real_sol: u64,
    pub tokens_sold: u64,
    pub complete: bool,
    pub bump: u8,
}

#[event]
pub struct TradeEvent {
    pub mint: Pubkey,
    pub user: Pubkey,
    pub is_buy: bool,
    pub sol: u64,
    pub tokens: u64,
    pub virtual_sol: u64,
    pub virtual_tokens: u64,
    pub ts: i64,
}

#[error_code]
pub enum CurveError {
    #[msg("Text is too long")]
    TextTooLong,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Price moved beyond your slippage limit")]
    Slippage,
    #[msg("The curve is complete: all tokens are sold")]
    CurveComplete,
}
