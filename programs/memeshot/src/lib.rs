use anchor_lang::prelude::*;
use anchor_lang::solana_program::pubkey;

declare_id!("5pueZCaJms1VB4uRuJf92fYpxMP8AxaHMTpYhqST4BUb");

// Pre-defined oracle authority
pub const ORACLE_AUTHORITY: Pubkey = pubkey!("7hJCvGkstBdYvG7gMU7iE9EeBhbk5uGdGTFQ6EfEBtF3");

#[program]
pub mod memeshot {
    use super::*;

    /// Initialize the program with oracle authority (one-time setup)
    pub fn initialize_program(ctx: Context<InitializeProgram>) -> Result<()> {
        let program_state = &mut ctx.accounts.program_state;
        program_state.oracle_authority = ORACLE_AUTHORITY;
        program_state.admin = ctx.accounts.admin.key();
        program_state.bump = ctx.bumps.program_state;
        
        emit!(ProgramInitialized {
            oracle_authority: ORACLE_AUTHORITY,
            admin: program_state.admin,
        });
        
        Ok(())
    }

    /// Update oracle authority (admin only)
    pub fn update_oracle_authority(
        ctx: Context<UpdateOracleAuthority>,
        new_oracle: Pubkey,
    ) -> Result<()> {
        let program_state = &mut ctx.accounts.program_state;
        
        require!(
            ctx.accounts.admin.key() == program_state.admin,
            TradingChallengeError::UnauthorizedAdmin
        );
        
        let old_oracle = program_state.oracle_authority;
        program_state.oracle_authority = new_oracle;
        
        emit!(OracleAuthorityUpdated {
            old_oracle,
            new_oracle,
            updated_by: ctx.accounts.admin.key(),
        });
        
        Ok(())
    }

    /// Creates a new trading challenge with SOL deposit
    pub fn create_challenge(
        ctx: Context<CreateChallenge>,
        stake_amount: u64, // SOL amount in lamports
        expires_at: i64,
    ) -> Result<()> {
        let challenge = &mut ctx.accounts.challenge_account;
        let clock = Clock::get()?;

        // Validate inputs
        require!(stake_amount > 0, TradingChallengeError::InvalidStakeAmount);
        require!(expires_at > clock.unix_timestamp, TradingChallengeError::InvalidExpiryTime);
        
        // Minimum stake requirement (e.g., 0.1 SOL = 100_000_000 lamports)
        require!(stake_amount >= 100_000_000, TradingChallengeError::MinimumStakeNotMet);

        // Initialize challenge account
        challenge.creator = ctx.accounts.creator.key();
        challenge.creator_stake_amount = stake_amount;
        challenge.pot_vault_pubkey = ctx.accounts.pot_vault.key();
        challenge.status = ChallengeStatus::Pending;
        challenge.expires_at = expires_at;
        challenge.created_at = clock.unix_timestamp;
        challenge.total_pot = stake_amount;
        challenge.bump = ctx.bumps.challenge_account;
        challenge.vault_bump = ctx.bumps.pot_vault;

        // Transfer SOL from creator to vault
        let cpi_context = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            anchor_lang::system_program::Transfer {
                from: ctx.accounts.creator.to_account_info(),
                to: ctx.accounts.pot_vault.to_account_info(),
            },
        );
        anchor_lang::system_program::transfer(cpi_context, stake_amount)?;

        emit!(ChallengeCreated {
            challenge_id: challenge.key(),
            creator: challenge.creator,
            stake_amount,
            expires_at,
        });

        Ok(())
    }

    /// Accepts an existing challenge with SOL deposit
    pub fn accept_challenge(
        ctx: Context<AcceptChallenge>,
        stake_amount: u64, // Must match creator's stake amount
    ) -> Result<()> {
        let challenge = &mut ctx.accounts.challenge_account;
        let clock = Clock::get()?;

        // Validate challenge state
        require!(challenge.status == ChallengeStatus::Pending, TradingChallengeError::ChallengeNotPending);
        require!(clock.unix_timestamp <= challenge.expires_at, TradingChallengeError::ChallengeNotExpired);
        require!(ctx.accounts.acceptor.key() != challenge.creator, TradingChallengeError::CreatorCannotAccept);
        require!(stake_amount == challenge.creator_stake_amount, TradingChallengeError::StakeMismatch);

        // Update challenge account
        challenge.acceptor_pubkey = Some(ctx.accounts.acceptor.key());
        challenge.acceptor_stake_amount = Some(stake_amount);
        challenge.status = ChallengeStatus::Active;
        challenge.start_timestamp = Some(clock.unix_timestamp);
        challenge.total_pot = challenge.creator_stake_amount + stake_amount;

        // Transfer SOL from acceptor to vault
        let cpi_context = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            anchor_lang::system_program::Transfer {
                from: ctx.accounts.acceptor.to_account_info(),
                to: ctx.accounts.pot_vault.to_account_info(),
            },
        );
        anchor_lang::system_program::transfer(cpi_context, stake_amount)?;

        emit!(ChallengeAccepted {
            challenge_id: challenge.key(),
            acceptor: ctx.accounts.acceptor.key(),
            stake_amount,
            start_timestamp: challenge.start_timestamp.unwrap(),
        });

        Ok(())
    }

    /// Claims payout after challenge completion - ORACLE ONLY
    pub fn claim_payout(
        ctx: Context<ClaimPayout>,
        winner_amount: u64,
        loser_amount: u64,
        pnl_data: PnlData, // Off-chain calculated PnL data
    ) -> Result<()> {
        let clock = Clock::get()?;
        let program_state = &ctx.accounts.program_state;

        // CRITICAL: Only oracle can trigger payout
        require!(
            ctx.accounts.oracle.key() == program_state.oracle_authority,
            TradingChallengeError::UnauthorizedOracle
        );
        require!(ctx.accounts.oracle.is_signer, TradingChallengeError::OracleSignatureRequired);

        // Store values we need before borrowing mutably
        let challenge_status = ctx.accounts.challenge_account.status;
        let total_pot = ctx.accounts.challenge_account.total_pot;
        let creator_key = ctx.accounts.challenge_account.creator;
        let acceptor_key = ctx.accounts.challenge_account.acceptor_pubkey;
        let challenge_key = ctx.accounts.challenge_account.key();

        // Validate challenge state
        require!(challenge_status == ChallengeStatus::Active, TradingChallengeError::ChallengeNotActive);
        require!(winner_amount + loser_amount == total_pot, TradingChallengeError::InvalidPayoutAmounts);

        let winner_key = ctx.accounts.winner.key();
        let loser_key = ctx.accounts.loser.key();

        // Validate winner and loser are participants
        require!(
            (winner_key == creator_key && loser_key == acceptor_key.unwrap()) ||
            (winner_key == acceptor_key.unwrap() && loser_key == creator_key),
            TradingChallengeError::InvalidParticipants
        );

        // Validate PnL data integrity (basic checks)
        require!(
            pnl_data.creator_pnl_percentage >= -100_00, // -100.00% max loss
            TradingChallengeError::InvalidPnlData
        );
        require!(
            pnl_data.acceptor_pnl_percentage >= -100_00,
            TradingChallengeError::InvalidPnlData
        );

        // Transfer SOL to winner
        if winner_amount > 0 {
            **ctx.accounts.pot_vault.to_account_info().try_borrow_mut_lamports()? -= winner_amount;
            **ctx.accounts.winner.to_account_info().try_borrow_mut_lamports()? += winner_amount;
        }

        // Transfer SOL to loser
        if loser_amount > 0 {
            **ctx.accounts.pot_vault.to_account_info().try_borrow_mut_lamports()? -= loser_amount;
            **ctx.accounts.loser.to_account_info().try_borrow_mut_lamports()? += loser_amount;
        }

        // Now borrow mutably to update challenge status
        let challenge = &mut ctx.accounts.challenge_account;
        challenge.status = ChallengeStatus::Completed;
        challenge.completed_at = Some(clock.unix_timestamp);
        challenge.winner_pubkey = Some(winner_key);
        challenge.winner_amount = Some(winner_amount);
        challenge.loser_amount = Some(loser_amount);
        challenge.final_pnl_data = Some(pnl_data);

        emit!(ChallengeCompleted {
            challenge_id: challenge_key,
            winner: winner_key,
            loser: loser_key,
            winner_amount,
            loser_amount,
            creator_pnl: pnl_data.creator_pnl_percentage,
            acceptor_pnl: pnl_data.acceptor_pnl_percentage,
            oracle: ctx.accounts.oracle.key(),
        });

        Ok(())
    }

    /// Cancels an expired challenge and refunds creator's SOL
    pub fn cancel_challenge(ctx: Context<CancelChallenge>) -> Result<()> {
        let clock = Clock::get()?;

        // Store values we need before borrowing mutably
        let challenge_status = ctx.accounts.challenge_account.status;
        let expires_at = ctx.accounts.challenge_account.expires_at;
        let creator_key = ctx.accounts.challenge_account.creator;
        let creator_stake_amount = ctx.accounts.challenge_account.creator_stake_amount;
        let challenge_key = ctx.accounts.challenge_account.key();

        // Validate challenge can be cancelled
        require!(challenge_status == ChallengeStatus::Pending, TradingChallengeError::ChallengeNotPending);
        require!(clock.unix_timestamp > expires_at, TradingChallengeError::ChallengeExpired);
        require!(ctx.accounts.creator.key() == creator_key, TradingChallengeError::UnauthorizedCancellation);

        // Refund creator's SOL from vault
        **ctx.accounts.pot_vault.to_account_info().try_borrow_mut_lamports()? -= creator_stake_amount;
        **ctx.accounts.creator.to_account_info().try_borrow_mut_lamports()? += creator_stake_amount;

        // Now borrow mutably to update challenge status
        let challenge = &mut ctx.accounts.challenge_account;
        challenge.status = ChallengeStatus::Cancelled;

        emit!(ChallengeCancelled {
            challenge_id: challenge_key,
            creator: creator_key,
            refund_amount: creator_stake_amount,
        });

        Ok(())
    }

    /// Closes a completed or cancelled challenge account to reclaim rent
    pub fn close_challenge(ctx: Context<CloseChallenge>) -> Result<()> {
        let challenge = &ctx.accounts.challenge_account;
        
        // Only allow closing if challenge is completed or cancelled
        require!(
            challenge.status == ChallengeStatus::Completed || challenge.status == ChallengeStatus::Cancelled,
            TradingChallengeError::ChallengeNotFinalized
        );

        // Only creator can close the challenge
        require!(
            ctx.accounts.creator.key() == challenge.creator,
            TradingChallengeError::UnauthorizedClosure
        );

        Ok(())
    }
}

// Account structures for oracle management
#[derive(Accounts)]
pub struct InitializeProgram<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + ProgramState::INIT_SPACE,
        seeds = [b"program_state"],
        bump
    )]
    pub program_state: Account<'info, ProgramState>,
    
    #[account(mut)]
    pub admin: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateOracleAuthority<'info> {
    #[account(
        mut,
        seeds = [b"program_state"],
        bump = program_state.bump
    )]
    pub program_state: Account<'info, ProgramState>,
    
    #[account(mut)]
    pub admin: Signer<'info>,
}

// SOL-based account structures
#[derive(Accounts)]
#[instruction(stake_amount: u64, expires_at: i64)]
pub struct CreateChallenge<'info> {
    #[account(
        init,
        payer = creator,
        space = 8 + ChallengeAccount::INIT_SPACE,
        seeds = [b"challenge", creator.key().as_ref()],
        bump
    )]
    pub challenge_account: Account<'info, ChallengeAccount>,

    /// CHECK: SOL vault PDA - verified by seeds constraint
    #[account(
        init,
        payer = creator,
        space = 0,
        seeds = [b"vault", challenge_account.key().as_ref()],
        bump,
    )]
    pub pot_vault: AccountInfo<'info>,

    #[account(mut)]
    pub creator: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(stake_amount: u64)]
pub struct AcceptChallenge<'info> {
    #[account(
        mut,
        seeds = [b"challenge", challenge_account.creator.as_ref()],
        bump = challenge_account.bump
    )]
    pub challenge_account: Account<'info, ChallengeAccount>,

    /// CHECK: SOL vault PDA - verified by seeds constraint
    #[account(
        mut,
        seeds = [b"vault", challenge_account.key().as_ref()],
        bump = challenge_account.vault_bump,
    )]
    pub pot_vault: AccountInfo<'info>,

    #[account(mut)]
    pub acceptor: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(winner_amount: u64, loser_amount: u64, pnl_data: PnlData)]
pub struct ClaimPayout<'info> {
    #[account(
        seeds = [b"program_state"],
        bump = program_state.bump
    )]
    pub program_state: Account<'info, ProgramState>,

    #[account(
        mut,
        seeds = [b"challenge", challenge_account.creator.as_ref()],
        bump = challenge_account.bump
    )]
    pub challenge_account: Account<'info, ChallengeAccount>,

    /// CHECK: SOL vault PDA - verified by seeds constraint
    #[account(
        mut,
        seeds = [b"vault", challenge_account.key().as_ref()],
        bump = challenge_account.vault_bump,
    )]
    pub pot_vault: AccountInfo<'info>,

    // Oracle must be signer
    pub oracle: Signer<'info>,

    /// CHECK: Verified in instruction logic - winner receives SOL
    #[account(mut)]
    pub winner: AccountInfo<'info>,

    /// CHECK: Verified in instruction logic - loser receives SOL
    #[account(mut)]
    pub loser: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelChallenge<'info> {
    #[account(
        mut,
        seeds = [b"challenge", challenge_account.creator.as_ref()],
        bump = challenge_account.bump
    )]
    pub challenge_account: Account<'info, ChallengeAccount>,

    /// CHECK: SOL vault PDA - verified by seeds constraint
    #[account(
        mut,
        seeds = [b"vault", challenge_account.key().as_ref()],
        bump = challenge_account.vault_bump,
    )]
    pub pot_vault: AccountInfo<'info>,

    #[account(mut)]
    pub creator: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseChallenge<'info> {
    #[account(
        mut,
        close = creator,
        seeds = [b"challenge", challenge_account.creator.as_ref()],
        bump = challenge_account.bump
    )]
    pub challenge_account: Account<'info, ChallengeAccount>,

    #[account(mut)]
    pub creator: Signer<'info>,
}

// Account data structures
#[account]
#[derive(InitSpace)]
pub struct ProgramState {
    pub oracle_authority: Pubkey,
    pub admin: Pubkey,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct ChallengeAccount {
    pub creator: Pubkey,
    pub creator_stake_amount: u64, // SOL amount in lamports
    pub acceptor_pubkey: Option<Pubkey>,
    pub acceptor_stake_amount: Option<u64>, // SOL amount in lamports
    pub pot_vault_pubkey: Pubkey,
    pub status: ChallengeStatus,
    pub expires_at: i64,
    pub created_at: i64,
    pub start_timestamp: Option<i64>,
    pub completed_at: Option<i64>,
    pub total_pot: u64, // Total SOL in lamports
    pub winner_pubkey: Option<Pubkey>,
    pub winner_amount: Option<u64>, // SOL amount in lamports
    pub loser_amount: Option<u64>, // SOL amount in lamports
    pub final_pnl_data: Option<PnlData>,
    pub bump: u8,
    pub vault_bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum ChallengeStatus {
    Pending,
    Active,
    Completed,
    Cancelled,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace)]
pub struct PnlData {
    pub creator_pnl_percentage: i32,    // e.g., 15000 = 150.00% gain, -5000 = 50.00% loss (basis points)
    pub acceptor_pnl_percentage: i32,
    pub calculation_timestamp: i64,
    pub data_source_hash: [u8; 32],     // Hash of off-chain data for verification
}

// Events
#[event]
pub struct ProgramInitialized {
    pub oracle_authority: Pubkey,
    pub admin: Pubkey,
}

#[event]
pub struct OracleAuthorityUpdated {
    pub old_oracle: Pubkey,
    pub new_oracle: Pubkey,
    pub updated_by: Pubkey,
}

#[event]
pub struct ChallengeCreated {
    pub challenge_id: Pubkey,
    pub creator: Pubkey,
    pub stake_amount: u64, // SOL in lamports
    pub expires_at: i64,
}

#[event]
pub struct ChallengeAccepted {
    pub challenge_id: Pubkey,
    pub acceptor: Pubkey,
    pub stake_amount: u64, // SOL in lamports
    pub start_timestamp: i64,
}

#[event]
pub struct ChallengeCompleted {
    pub challenge_id: Pubkey,
    pub winner: Pubkey,
    pub loser: Pubkey,
    pub winner_amount: u64, // SOL in lamports
    pub loser_amount: u64, // SOL in lamports
    pub creator_pnl: i32,
    pub acceptor_pnl: i32,
    pub oracle: Pubkey,
}

#[event]
pub struct ChallengeCancelled {
    pub challenge_id: Pubkey,
    pub creator: Pubkey,
    pub refund_amount: u64, // SOL in lamports
}

#[error_code]
pub enum TradingChallengeError {
    #[msg("Invalid stake amount")]
    InvalidStakeAmount,
    #[msg("Minimum stake not met (0.1 SOL required)")]
    MinimumStakeNotMet,
    #[msg("Invalid expiry time")]
    InvalidExpiryTime,
    #[msg("Challenge is not in pending status")]
    ChallengeNotPending,
    #[msg("Challenge has not expired yet")]
    ChallengeNotExpired,
    #[msg("Challenge has expired")]
    ChallengeExpired,
    #[msg("Creator cannot accept their own challenge")]
    CreatorCannotAccept,
    #[msg("Stake amount mismatch")]
    StakeMismatch,
    #[msg("Challenge is not active")]
    ChallengeNotActive,
    #[msg("Invalid payout amounts")]
    InvalidPayoutAmounts,
    #[msg("Invalid participants")]
    InvalidParticipants,
    #[msg("Unauthorized cancellation")]
    UnauthorizedCancellation,
    #[msg("Challenge is not finalized")]
    ChallengeNotFinalized,
    #[msg("Unauthorized closure")]
    UnauthorizedClosure,
    #[msg("Unauthorized oracle")]
    UnauthorizedOracle,
    #[msg("Oracle signature required")]
    OracleSignatureRequired,
    #[msg("Unauthorized admin")]
    UnauthorizedAdmin,
    #[msg("Invalid PnL data")]
    InvalidPnlData,
}