//! Program state processor

use {
    crate::{
        error::StakePoolError,
        find_deposit_authority_program_address,
        instruction::{FundingType, PreferredValidatorType, StakePoolInstruction},
        minimum_delegation, minimum_reserve_lamports, minimum_stake_lamports,
        state::{
            is_extension_supported_for_mint, AccountType, Fee, FeeType, StakePool, StakeStatus,
            StakeWithdrawSource, ValidatorList, ValidatorListHeader, ValidatorStakeInfo,
        },
        AUTHORITY_DEPOSIT, AUTHORITY_WITHDRAW, TRANSIENT_STAKE_SEED_PREFIX,
    },
    borsh::{BorshDeserialize, BorshSerialize},
    mpl_token_metadata::{
        instruction::{create_metadata_accounts_v3, update_metadata_accounts_v2},
        pda::find_metadata_account,
        state::DataV2,
    },
    num_traits::FromPrimitive,
    solana_program::{
        account_info::{next_account_info, AccountInfo},
        borsh::try_from_slice_unchecked,
        clock::{Clock, Epoch},
        decode_error::DecodeError,
        entrypoint::ProgramResult,
        msg,
        program::{invoke, invoke_signed},
        program_error::{PrintProgramError, ProgramError},
        pubkey::Pubkey,
        rent::Rent,
        stake, system_instruction, system_program,
        sysvar::Sysvar,
    },
    spl_token_2022::{
        check_spl_token_program_account,
        extension::{BaseStateWithExtensions, StateWithExtensions},
        state::Mint,
    },
    std::num::NonZeroU32,
};

/// Deserialize the stake state from AccountInfo
fn get_stake_state(
    stake_account_info: &AccountInfo,
) -> Result<(stake::state::Meta, stake::state::Stake), ProgramError> {
    let stake_state =
        try_from_slice_unchecked::<stake::state::StakeState>(&stake_account_info.data.borrow())?;
    match stake_state {
        stake::state::StakeState::Stake(meta, stake) => Ok((meta, stake)),
        _ => Err(StakePoolError::WrongStakeState.into()),
    }
}

/// Check validity of vote address for a particular stake account
fn check_validator_stake_address(
    program_id: &Pubkey,
    stake_pool_address: &Pubkey,
    stake_account_address: &Pubkey,
    vote_address: &Pubkey,
    seed: Option<NonZeroU32>,
) -> Result<(), ProgramError> {
    // Check stake account address validity
    let (validator_stake_address, _) =
        crate::find_stake_program_address(program_id, vote_address, stake_pool_address, seed);
    if validator_stake_address != *stake_account_address {
        msg!(
            "Incorrect stake account address for vote {}, expected {}, received {}",
            vote_address,
            validator_stake_address,
            stake_account_address
        );
        Err(StakePoolError::InvalidStakeAccountAddress.into())
    } else {
        Ok(())
    }
}

/// Check validity of vote address for a particular stake account
fn check_transient_stake_address(
    program_id: &Pubkey,
    stake_pool_address: &Pubkey,
    stake_account_address: &Pubkey,
    vote_address: &Pubkey,
    seed: u64,
) -> Result<u8, ProgramError> {
    // Check stake account address validity
    let (transient_stake_address, bump_seed) = crate::find_transient_stake_program_address(
        program_id,
        vote_address,
        stake_pool_address,
        seed,
    );
    if transient_stake_address != *stake_account_address {
        Err(StakePoolError::InvalidStakeAccountAddress.into())
    } else {
        Ok(bump_seed)
    }
}

/// Check mpl metadata account address for the pool mint
fn check_mpl_metadata_account_address(
    metadata_address: &Pubkey,
    pool_mint: &Pubkey,
) -> Result<(), ProgramError> {
    let (metadata_account_pubkey, _) = find_metadata_account(pool_mint);
    if metadata_account_pubkey != *metadata_address {
        Err(StakePoolError::InvalidMetadataAccount.into())
    } else {
        Ok(())
    }
}

/// Check system program address
fn check_system_program(program_id: &Pubkey) -> Result<(), ProgramError> {
    if *program_id != system_program::id() {
        msg!(
            "Expected system program {}, received {}",
            system_program::id(),
            program_id
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Check stake program address
fn check_stake_program(program_id: &Pubkey) -> Result<(), ProgramError> {
    if *program_id != stake::program::id() {
        msg!(
            "Expected stake program {}, received {}",
            stake::program::id(),
            program_id
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Check mpl metadata program
fn check_mpl_metadata_program(program_id: &Pubkey) -> Result<(), ProgramError> {
    if *program_id != mpl_token_metadata::id() {
        msg!(
            "Expected mpl metadata program {}, received {}",
            mpl_token_metadata::id(),
            program_id
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Check rent sysvar correctness
fn check_rent_sysvar(sysvar_key: &Pubkey) -> Result<(), ProgramError> {
    if *sysvar_key != solana_program::sysvar::rent::id() {
        msg!(
            "Expected rent sysvar {}, received {}",
            solana_program::sysvar::rent::id(),
            sysvar_key
        );
        Err(ProgramError::InvalidArgument)
    } else {
        Ok(())
    }
}

/// Check account owner is the given program
fn check_account_owner(
    account_info: &AccountInfo,
    program_id: &Pubkey,
) -> Result<(), ProgramError> {
    if *program_id != *account_info.owner {
        msg!(
            "Expected account to be owned by program {}, received {}",
            program_id,
            account_info.owner
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Checks if a stake acount can be managed by the pool
fn stake_is_usable_by_pool(
    meta: &stake::state::Meta,
    expected_authority: &Pubkey,
    expected_lockup: &stake::state::Lockup,
) -> bool {
    meta.authorized.staker == *expected_authority
        && meta.authorized.withdrawer == *expected_authority
        && meta.lockup == *expected_lockup
}

/// Checks if a stake acount is active, without taking into account cooldowns
fn stake_is_inactive_without_history(stake: &stake::state::Stake, epoch: Epoch) -> bool {
    stake.delegation.deactivation_epoch < epoch
        || (stake.delegation.activation_epoch == epoch
            && stake.delegation.deactivation_epoch == epoch)
}

/// Create a stake account on a PDA without transferring lamports
fn create_stake_account<'a>(
    stake_account_info: AccountInfo<'a>,
    stake_account_signer_seeds: &[&[u8]],
    system_program_info: AccountInfo<'a>,
) -> Result<(), ProgramError> {
    invoke_signed(
        &system_instruction::allocate(
            stake_account_info.key,
            std::mem::size_of::<stake::state::StakeState>() as u64,
        ),
        &[stake_account_info.clone(), system_program_info.clone()],
        &[stake_account_signer_seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(stake_account_info.key, &stake::program::id()),
        &[stake_account_info, system_program_info],
        &[stake_account_signer_seeds],
    )
}

/// Program state handler.
pub struct Processor {}
impl Processor {
    /// Issue a delegate_stake instruction.
    #[allow(clippy::too_many_arguments)]
    fn stake_delegate<'a>(
        stake_info: AccountInfo<'a>,
        vote_account_info: AccountInfo<'a>,
        clock_info: AccountInfo<'a>,
        stake_history_info: AccountInfo<'a>,
        stake_config_info: AccountInfo<'a>,
        authority_info: AccountInfo<'a>,
        stake_pool: &Pubkey,
        authority_type: &[u8],
        bump_seed: u8,
    ) -> Result<(), ProgramError> {
        let authority_signature_seeds =
            [&stake_pool.to_bytes()[..32], authority_type, &[bump_seed]];
        let signers = &[&authority_signature_seeds[..]];

        let ix = stake::instruction::delegate_stake(
            stake_info.key,
            authority_info.key,
            vote_account_info.key,
        );

        invoke_signed(
            &ix,
            &[
                stake_info,
                vote_account_info,
                clock_info,
                stake_history_info,
                stake_config_info,
                authority_info,
            ],
            signers,
        )
    }

    /// Issue a stake_deactivate instruction.
    fn stake_deactivate<'a>(
        stake_info: AccountInfo<'a>,
        clock_info: AccountInfo<'a>,
        authority_info: AccountInfo<'a>,
        stake_pool: &Pubkey,
        authority_type: &[u8],
        bump_seed: u8,
    ) -> Result<(), ProgramError> {
        let authority_signature_seeds =
            [&stake_pool.to_bytes()[..32], authority_type, &[bump_seed]];
        let signers = &[&authority_signature_seeds[..]];

        let ix = stake::instruction::deactivate_stake(stake_info.key, authority_info.key);

        invoke_signed(&ix, &[stake_info, clock_info, authority_info], signers)
    }

    /// Issue a stake_split instruction.
    fn stake_split<'a>(
        stake_pool: &Pubkey,
        stake_account: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        authority_type: &[u8],
        bump_seed: u8,
        amount: u64,
        split_stake: AccountInfo<'a>,
    ) -> Result<(), ProgramError> {
        let me_bytes = stake_pool.to_bytes();
        let authority_signature_seeds = [&me_bytes[..32], authority_type, &[bump_seed]];
        let signers = &[&authority_signature_seeds[..]];

        let split_instruction =
            stake::instruction::split(stake_account.key, authority.key, amount, split_stake.key);

        invoke_signed(
            split_instruction.last().unwrap(),
            &[stake_account, split_stake, authority],
            signers,
        )
    }

    /// Issue a stake_merge instruction.
    #[allow(clippy::too_many_arguments)]
    fn stake_merge<'a>(
        stake_pool: &Pubkey,
        source_account: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        authority_type: &[u8],
        bump_seed: u8,
        destination_account: AccountInfo<'a>,
        clock: AccountInfo<'a>,
        stake_history: AccountInfo<'a>,
        stake_program_info: AccountInfo<'a>,
    ) -> Result<(), ProgramError> {
        let me_bytes = stake_pool.to_bytes();
        let authority_signature_seeds = [&me_bytes[..32], authority_type, &[bump_seed]];
        let signers = &[&authority_signature_seeds[..]];

        let merge_instruction =
            stake::instruction::merge(destination_account.key, source_account.key, authority.key);

        invoke_signed(
            &merge_instruction[0],
            &[
                destination_account,
                source_account,
                clock,
                stake_history,
                authority,
                stake_program_info,
            ],
            signers,
        )
    }

    /// Issue stake::instruction::authorize instructions to update both authorities
    fn stake_authorize<'a>(
        stake_account: AccountInfo<'a>,
        stake_authority: AccountInfo<'a>,
        new_stake_authority: &Pubkey,
        clock: AccountInfo<'a>,
        stake_program_info: AccountInfo<'a>,
    ) -> Result<(), ProgramError> {
        let authorize_instruction = stake::instruction::authorize(
            stake_account.key,
            stake_authority.key,
            new_stake_authority,
            stake::state::StakeAuthorize::Staker,
            None,
        );

        invoke(
            &authorize_instruction,
            &[
                stake_account.clone(),
                clock.clone(),
                stake_authority.clone(),
                stake_program_info.clone(),
            ],
        )?;

        let authorize_instruction = stake::instruction::authorize(
            stake_account.key,
            stake_authority.key,
            new_stake_authority,
            stake::state::StakeAuthorize::Withdrawer,
            None,
        );

        invoke(
            &authorize_instruction,
            &[stake_account, clock, stake_authority, stake_program_info],
        )
    }

    /// Issue stake::instruction::authorize instructions to update both authorities
    #[allow(clippy::too_many_arguments)]
    fn stake_authorize_signed<'a>(
        stake_pool: &Pubkey,
        stake_account: AccountInfo<'a>,
        stake_authority: AccountInfo<'a>,
        authority_type: &[u8],
        bump_seed: u8,
        new_stake_authority: &Pubkey,
        clock: AccountInfo<'a>,
        stake_program_info: AccountInfo<'a>,
    ) -> Result<(), ProgramError> {
        let me_bytes = stake_pool.to_bytes();
        let authority_signature_seeds = [&me_bytes[..32], authority_type, &[bump_seed]];
        let signers = &[&authority_signature_seeds[..]];

        let authorize_instruction = stake::instruction::authorize(
            stake_account.key,
            stake_authority.key,
            new_stake_authority,
            stake::state::StakeAuthorize::Staker,
            None,
        );

        invoke_signed(
            &authorize_instruction,
            &[
                stake_account.clone(),
                clock.clone(),
                stake_authority.clone(),
                stake_program_info.clone(),
            ],
            signers,
        )?;

        let authorize_instruction = stake::instruction::authorize(
            stake_account.key,
            stake_authority.key,
            new_stake_authority,
            stake::state::StakeAuthorize::Withdrawer,
            None,
        );
        invoke_signed(
            &authorize_instruction,
            &[stake_account, clock, stake_authority, stake_program_info],
            signers,
        )
    }

    /// Issue stake::instruction::withdraw instruction to move additional lamports
    #[allow(clippy::too_many_arguments)]
    fn stake_withdraw<'a>(
        stake_pool: &Pubkey,
        source_account: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        authority_type: &[u8],
        bump_seed: u8,
        destination_account: AccountInfo<'a>,
        clock: AccountInfo<'a>,
        stake_history: AccountInfo<'a>,
        stake_program_info: AccountInfo<'a>,
        lamports: u64,
    ) -> Result<(), ProgramError> {
        let me_bytes = stake_pool.to_bytes();
        let authority_signature_seeds = [&me_bytes[..32], authority_type, &[bump_seed]];
        let signers = &[&authority_signature_seeds[..]];
        let custodian_pubkey = None;

        let withdraw_instruction = stake::instruction::withdraw(
            source_account.key,
            authority.key,
            destination_account.key,
            lamports,
            custodian_pubkey,
        );

        invoke_signed(
            &withdraw_instruction,
            &[
                source_account,
                destination_account,
                clock,
                stake_history,
                authority,
                stake_program_info,
            ],
            signers,
        )
    }

    /// Issue a spl_token `Burn` instruction.
    #[allow(clippy::too_many_arguments)]
    fn token_burn<'a>(
        token_program: AccountInfo<'a>,
        burn_account: AccountInfo<'a>,
        mint: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let ix = spl_token_2022::instruction::burn(
            token_program.key,
            burn_account.key,
            mint.key,
            authority.key,
            &[],
            amount,
        )?;

        invoke(&ix, &[burn_account, mint, authority, token_program])
    }

    /// Issue a spl_token `MintTo` instruction.
    #[allow(clippy::too_many_arguments)]
    fn token_mint_to<'a>(
        stake_pool: &Pubkey,
        token_program: AccountInfo<'a>,
        mint: AccountInfo<'a>,
        destination: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        authority_type: &[u8],
        bump_seed: u8,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let me_bytes = stake_pool.to_bytes();
        let authority_signature_seeds = [&me_bytes[..32], authority_type, &[bump_seed]];
        let signers = &[&authority_signature_seeds[..]];

        let ix = spl_token_2022::instruction::mint_to(
            token_program.key,
            mint.key,
            destination.key,
            authority.key,
            &[],
            amount,
        )?;

        invoke_signed(&ix, &[mint, destination, authority, token_program], signers)
    }

    /// Issue a spl_token `Transfer` instruction.
    #[allow(clippy::too_many_arguments)]
    fn token_transfer<'a>(
        token_program: AccountInfo<'a>,
        source: AccountInfo<'a>,
        mint: AccountInfo<'a>,
        destination: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        amount: u64,
        decimals: u8,
    ) -> Result<(), ProgramError> {
        let ix = spl_token_2022::instruction::transfer_checked(
            token_program.key,
            source.key,
            mint.key,
            destination.key,
            authority.key,
            &[],
            amount,
            decimals,
        )?;
        invoke(&ix, &[source, mint, destination, authority, token_program])
    }

    fn sol_transfer<'a>(
        source: AccountInfo<'a>,
        destination: AccountInfo<'a>,
        system_program: AccountInfo<'a>,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let ix = solana_program::system_instruction::transfer(source.key, destination.key, amount);
        invoke(&ix, &[source, destination, system_program])
    }

    /// Processes `Initialize` instruction.
    #[inline(never)] // needed due to stack size violation
    fn process_initialize(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        epoch_fee: Fee,
        withdrawal_fee: Fee,
        deposit_fee: Fee,
        referral_fee: u8,
        max_validators: u32,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let manager_info = next_account_info(account_info_iter)?;
        let staker_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let reserve_stake_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let manager_fee_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let rent = Rent::get()?;

        if !manager_info.is_signer {
            msg!("Manager did not sign initialization");
            return Err(StakePoolError::SignatureMissing.into());
        }

        if stake_pool_info.key == validator_list_info.key {
            msg!("Cannot use same account for stake pool and validator list");
            return Err(StakePoolError::AlreadyInUse.into());
        }

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_uninitialized() {
            msg!("Provided stake pool already in use");
            return Err(StakePoolError::AlreadyInUse.into());
        }

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list =
            try_from_slice_unchecked::<ValidatorList>(&validator_list_info.data.borrow())?;
        if !validator_list.header.is_uninitialized() {
            msg!("Provided validator list already in use");
            return Err(StakePoolError::AlreadyInUse.into());
        }

        let data_length = validator_list_info.data_len();
        let expected_max_validators = ValidatorList::calculate_max_validators(data_length);
        if expected_max_validators != max_validators as usize || max_validators == 0 {
            msg!(
                "Incorrect validator list size provided, expected {}, provided {}",
                expected_max_validators,
                max_validators
            );
            return Err(StakePoolError::UnexpectedValidatorListAccountSize.into());
        }
        validator_list.header.account_type = AccountType::ValidatorList;
        validator_list.header.max_validators = max_validators;
        validator_list.validators.clear();

        if !rent.is_exempt(stake_pool_info.lamports(), stake_pool_info.data_len()) {
            msg!("Stake pool not rent-exempt");
            return Err(ProgramError::AccountNotRentExempt);
        }

        if !rent.is_exempt(
            validator_list_info.lamports(),
            validator_list_info.data_len(),
        ) {
            msg!("Validator stake list not rent-exempt");
            return Err(ProgramError::AccountNotRentExempt);
        }

        // Numerator should be smaller than or equal to denominator (fee <= 1)
        if epoch_fee.numerator > epoch_fee.denominator
            || withdrawal_fee.numerator > withdrawal_fee.denominator
            || deposit_fee.numerator > deposit_fee.denominator
            || referral_fee > 100u8
        {
            return Err(StakePoolError::FeeTooHigh.into());
        }

        check_spl_token_program_account(token_program_info.key)?;

        if pool_mint_info.owner != token_program_info.key {
            return Err(ProgramError::IncorrectProgramId);
        }

        stake_pool.token_program_id = *token_program_info.key;
        stake_pool.pool_mint = *pool_mint_info.key;

        let (stake_deposit_authority, sol_deposit_authority) =
            match next_account_info(account_info_iter) {
                Ok(deposit_authority_info) => (
                    *deposit_authority_info.key,
                    Some(*deposit_authority_info.key),
                ),
                Err(_) => (
                    find_deposit_authority_program_address(program_id, stake_pool_info.key).0,
                    None,
                ),
            };
        let (withdraw_authority_key, stake_withdraw_bump_seed) =
            crate::find_withdraw_authority_program_address(program_id, stake_pool_info.key);
        if withdraw_authority_key != *withdraw_authority_info.key {
            msg!(
                "Incorrect withdraw authority provided, expected {}, received {}",
                withdraw_authority_key,
                withdraw_authority_info.key
            );
            return Err(StakePoolError::InvalidProgramAddress.into());
        }

        {
            let pool_mint_data = pool_mint_info.try_borrow_data()?;
            let pool_mint = StateWithExtensions::<Mint>::unpack(&pool_mint_data)?;

            if pool_mint.base.supply != 0 {
                return Err(StakePoolError::NonZeroPoolTokenSupply.into());
            }

            if !pool_mint
                .base
                .mint_authority
                .contains(&withdraw_authority_key)
            {
                return Err(StakePoolError::WrongMintingAuthority.into());
            }

            if pool_mint.base.freeze_authority.is_some() {
                return Err(StakePoolError::InvalidMintFreezeAuthority.into());
            }

            let extensions = pool_mint.get_extension_types()?;
            if extensions
                .iter()
                .any(|x| !is_extension_supported_for_mint(x))
            {
                return Err(StakePoolError::UnsupportedMintExtension.into());
            }
        }
        stake_pool.check_manager_fee_info(manager_fee_info)?;

        if *reserve_stake_info.owner != stake::program::id() {
            msg!("Reserve stake account not owned by stake program");
            return Err(ProgramError::IncorrectProgramId);
        }
        let stake_state = try_from_slice_unchecked::<stake::state::StakeState>(
            &reserve_stake_info.data.borrow(),
        )?;
        let total_lamports = if let stake::state::StakeState::Initialized(meta) = stake_state {
            if meta.lockup != stake::state::Lockup::default() {
                msg!("Reserve stake account has some lockup");
                return Err(StakePoolError::WrongStakeState.into());
            }

            if meta.authorized.staker != withdraw_authority_key {
                msg!(
                    "Reserve stake account has incorrect staker {}, should be {}",
                    meta.authorized.staker,
                    withdraw_authority_key
                );
                return Err(StakePoolError::WrongStakeState.into());
            }

            if meta.authorized.withdrawer != withdraw_authority_key {
                msg!(
                    "Reserve stake account has incorrect withdrawer {}, should be {}",
                    meta.authorized.staker,
                    withdraw_authority_key
                );
                return Err(StakePoolError::WrongStakeState.into());
            }
            reserve_stake_info
                .lamports()
                .checked_sub(minimum_reserve_lamports(&meta))
                .ok_or(StakePoolError::CalculationFailure)?
        } else {
            msg!("Reserve stake account not in intialized state");
            return Err(StakePoolError::WrongStakeState.into());
        };

        if total_lamports > 0 {
            Self::token_mint_to(
                stake_pool_info.key,
                token_program_info.clone(),
                pool_mint_info.clone(),
                manager_fee_info.clone(),
                withdraw_authority_info.clone(),
                AUTHORITY_WITHDRAW,
                stake_withdraw_bump_seed,
                total_lamports,
            )?;
        }

        validator_list.serialize(&mut *validator_list_info.data.borrow_mut())?;

        stake_pool.account_type = AccountType::StakePool;
        stake_pool.manager = *manager_info.key;
        stake_pool.staker = *staker_info.key;
        stake_pool.stake_deposit_authority = stake_deposit_authority;
        stake_pool.stake_withdraw_bump_seed = stake_withdraw_bump_seed;
        stake_pool.validator_list = *validator_list_info.key;
        stake_pool.reserve_stake = *reserve_stake_info.key;
        stake_pool.manager_fee_account = *manager_fee_info.key;
        stake_pool.total_lamports = total_lamports;
        stake_pool.pool_token_supply = total_lamports;
        stake_pool.last_update_epoch = Clock::get()?.epoch;
        stake_pool.lockup = stake::state::Lockup::default();
        stake_pool.epoch_fee = epoch_fee;
        stake_pool.next_epoch_fee = None;
        stake_pool.preferred_deposit_validator_vote_address = None;
        stake_pool.preferred_withdraw_validator_vote_address = None;
        stake_pool.stake_deposit_fee = deposit_fee;
        stake_pool.stake_withdrawal_fee = withdrawal_fee;
        stake_pool.next_stake_withdrawal_fee = None;
        stake_pool.stake_referral_fee = referral_fee;
        stake_pool.sol_deposit_authority = sol_deposit_authority;
        stake_pool.sol_deposit_fee = deposit_fee;
        stake_pool.sol_referral_fee = referral_fee;
        stake_pool.sol_withdraw_authority = None;
        stake_pool.sol_withdrawal_fee = withdrawal_fee;
        stake_pool.next_sol_withdrawal_fee = None;
        stake_pool.last_epoch_pool_token_supply = 0;
        stake_pool.last_epoch_total_lamports = 0;

        stake_pool
            .serialize(&mut *stake_pool_info.data.borrow_mut())
            .map_err(|e| e.into())
    }

    /// Processes `AddValidatorToPool` instruction.
    #[inline(never)] // needed due to stack size violation
    fn process_add_validator_to_pool(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        raw_validator_seed: u32,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let staker_info = next_account_info(account_info_iter)?;
        let reserve_stake_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let stake_info = next_account_info(account_info_iter)?;
        let validator_vote_info = next_account_info(account_info_iter)?;
        let rent_info = next_account_info(account_info_iter)?;
        let rent = &Rent::from_account_info(rent_info)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let stake_history_info = next_account_info(account_info_iter)?;
        let stake_config_info = next_account_info(account_info_iter)?;
        let system_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_system_program(system_program_info.key)?;
        check_stake_program(stake_program_info.key)?;

        check_account_owner(stake_pool_info, program_id)?;
        let stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;

        stake_pool.check_staker(staker_info)?;
        stake_pool.check_reserve_stake(reserve_stake_info)?;
        stake_pool.check_validator_list(validator_list_info)?;

        if stake_pool.last_update_epoch < clock.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, mut validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }
        if header.max_validators == validator_list.len() {
            return Err(ProgramError::AccountDataTooSmall);
        }
        let maybe_validator_stake_info = validator_list.find::<ValidatorStakeInfo, _>(|x| {
            ValidatorStakeInfo::memcmp_pubkey(x, validator_vote_info.key)
        });
        if maybe_validator_stake_info.is_some() {
            return Err(StakePoolError::ValidatorAlreadyAdded.into());
        }

        let validator_seed = NonZeroU32::new(raw_validator_seed);
        let (stake_address, bump_seed) = crate::find_stake_program_address(
            program_id,
            validator_vote_info.key,
            stake_pool_info.key,
            validator_seed,
        );
        if stake_address != *stake_info.key {
            return Err(StakePoolError::InvalidStakeAccountAddress.into());
        }

        let validator_seed_bytes = validator_seed.map(|s| s.get().to_le_bytes());
        let stake_account_signer_seeds: &[&[_]] = &[
            validator_vote_info.key.as_ref(),
            stake_pool_info.key.as_ref(),
            validator_seed_bytes
                .as_ref()
                .map(|s| s.as_slice())
                .unwrap_or(&[]),
            &[bump_seed],
        ];

        // Fund the stake account with the minimum + rent-exempt balance
        let space = std::mem::size_of::<stake::state::StakeState>();
        let stake_minimum_delegation = stake::tools::get_minimum_delegation()?;
        let required_lamports = minimum_delegation(stake_minimum_delegation)
            .saturating_add(rent.minimum_balance(space));

        // Check that we're not draining the reserve totally
        let reserve_stake = try_from_slice_unchecked::<stake::state::StakeState>(
            &reserve_stake_info.data.borrow(),
        )?;
        let reserve_meta = reserve_stake
            .meta()
            .ok_or(StakePoolError::WrongStakeState)?;
        let minimum_lamports = minimum_reserve_lamports(&reserve_meta);
        let reserve_lamports = reserve_stake_info.lamports();
        if reserve_lamports.saturating_sub(required_lamports) < minimum_lamports {
            msg!(
                "Need to add {} lamports for the reserve stake to be rent-exempt after adding a validator, reserve currently has {} lamports",
                required_lamports.saturating_add(minimum_lamports).saturating_sub(reserve_lamports),
                reserve_lamports
            );
            return Err(ProgramError::InsufficientFunds);
        }

        // Create new stake account
        create_stake_account(
            stake_info.clone(),
            stake_account_signer_seeds,
            system_program_info.clone(),
        )?;
        // split into validator stake account
        Self::stake_split(
            stake_pool_info.key,
            reserve_stake_info.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            required_lamports,
            stake_info.clone(),
        )?;

        Self::stake_delegate(
            stake_info.clone(),
            validator_vote_info.clone(),
            clock_info.clone(),
            stake_history_info.clone(),
            stake_config_info.clone(),
            withdraw_authority_info.clone(),
            stake_pool_info.key,
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
        )?;

        validator_list.push(ValidatorStakeInfo {
            status: StakeStatus::Active,
            vote_account_address: *validator_vote_info.key,
            active_stake_lamports: required_lamports,
            transient_stake_lamports: 0,
            last_update_epoch: clock.epoch,
            transient_seed_suffix: 0,
            unused: 0,
            validator_seed_suffix: raw_validator_seed,
        })?;

        Ok(())
    }

    /// Processes `RemoveValidatorFromPool` instruction.
    #[inline(never)] // needed due to stack size violation
    fn process_remove_validator_from_pool(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let staker_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let stake_account_info = next_account_info(account_info_iter)?;
        let transient_stake_account_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_stake_program(stake_program_info.key)?;
        check_account_owner(stake_pool_info, program_id)?;

        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_staker(staker_info)?;

        if stake_pool.last_update_epoch < clock.epoch {
            msg!(
                "clock {} pool {}",
                clock.epoch,
                stake_pool.last_update_epoch
            );
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        stake_pool.check_validator_list(validator_list_info)?;

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, mut validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let (meta, stake) = get_stake_state(stake_account_info)?;
        let vote_account_address = stake.delegation.voter_pubkey;
        let maybe_validator_stake_info = validator_list.find_mut::<ValidatorStakeInfo, _>(|x| {
            ValidatorStakeInfo::memcmp_pubkey(x, &vote_account_address)
        });
        if maybe_validator_stake_info.is_none() {
            msg!(
                "Vote account {} not found in stake pool",
                vote_account_address
            );
            return Err(StakePoolError::ValidatorNotFound.into());
        }
        let mut validator_stake_info = maybe_validator_stake_info.unwrap();
        check_validator_stake_address(
            program_id,
            stake_pool_info.key,
            stake_account_info.key,
            &vote_account_address,
            NonZeroU32::new(validator_stake_info.validator_seed_suffix),
        )?;

        if validator_stake_info.status != StakeStatus::Active {
            msg!("Validator is already marked for removal");
            return Err(StakePoolError::ValidatorNotFound.into());
        }

        let stake_lamports = **stake_account_info.lamports.borrow();
        let stake_minimum_delegation = stake::tools::get_minimum_delegation()?;
        let required_lamports = minimum_stake_lamports(&meta, stake_minimum_delegation);
        if stake_lamports != required_lamports {
            msg!(
                "Attempting to remove validator account with {} lamports, must have {} lamports",
                stake_lamports,
                required_lamports
            );
            return Err(StakePoolError::StakeLamportsNotEqualToMinimum.into());
        }

        let current_minimum_delegation = minimum_delegation(stake_minimum_delegation);
        if stake.delegation.stake != current_minimum_delegation {
            msg!(
                "Error: attempting to remove stake with delegation of {} lamports, must have {} lamports",
                stake.delegation.stake,
                current_minimum_delegation
            );
            return Err(StakePoolError::StakeLamportsNotEqualToMinimum.into());
        }

        let new_status = if validator_stake_info.transient_stake_lamports > 0 {
            check_transient_stake_address(
                program_id,
                stake_pool_info.key,
                transient_stake_account_info.key,
                &vote_account_address,
                validator_stake_info.transient_seed_suffix,
            )?;

            match get_stake_state(transient_stake_account_info) {
                Ok((meta, stake))
                    if stake_is_usable_by_pool(
                        &meta,
                        withdraw_authority_info.key,
                        &stake_pool.lockup,
                    ) =>
                {
                    if stake.delegation.deactivation_epoch == Epoch::MAX {
                        msg!(
                            "Transient stake {} activating, can't remove stake {} on validator {}",
                            transient_stake_account_info.key,
                            stake_account_info.key,
                            vote_account_address
                        );
                        return Err(StakePoolError::WrongStakeState.into());
                    } else {
                        StakeStatus::DeactivatingAll
                    }
                }
                _ => StakeStatus::DeactivatingValidator,
            }
        } else {
            StakeStatus::DeactivatingValidator
        };

        // deactivate stake
        Self::stake_deactivate(
            stake_account_info.clone(),
            clock_info.clone(),
            withdraw_authority_info.clone(),
            stake_pool_info.key,
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
        )?;

        validator_stake_info.status = new_status;

        if stake_pool.preferred_deposit_validator_vote_address == Some(vote_account_address) {
            stake_pool.preferred_deposit_validator_vote_address = None;
        }
        if stake_pool.preferred_withdraw_validator_vote_address == Some(vote_account_address) {
            stake_pool.preferred_withdraw_validator_vote_address = None;
        }
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes `DecreaseValidatorStake` instruction.
    #[inline(never)] // needed due to stack size violation
    fn process_decrease_validator_stake(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        lamports: u64,
        transient_stake_seed: u64,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let staker_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let validator_stake_account_info = next_account_info(account_info_iter)?;
        let transient_stake_account_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let rent_info = next_account_info(account_info_iter)?;
        let rent = &Rent::from_account_info(rent_info)?;
        let system_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_system_program(system_program_info.key)?;
        check_stake_program(stake_program_info.key)?;
        check_account_owner(stake_pool_info, program_id)?;

        let stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            msg!("Expected valid stake pool");
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_staker(staker_info)?;

        if stake_pool.last_update_epoch < clock.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        stake_pool.check_validator_list(validator_list_info)?;
        check_account_owner(validator_list_info, program_id)?;
        let validator_list_data = &mut *validator_list_info.data.borrow_mut();
        let (validator_list_header, mut validator_list) =
            ValidatorListHeader::deserialize_vec(validator_list_data)?;
        if !validator_list_header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let (meta, stake) = get_stake_state(validator_stake_account_info)?;
        let vote_account_address = stake.delegation.voter_pubkey;

        let maybe_validator_stake_info = validator_list.find_mut::<ValidatorStakeInfo, _>(|x| {
            ValidatorStakeInfo::memcmp_pubkey(x, &vote_account_address)
        });
        if maybe_validator_stake_info.is_none() {
            msg!(
                "Vote account {} not found in stake pool",
                vote_account_address
            );
            return Err(StakePoolError::ValidatorNotFound.into());
        }
        let mut validator_stake_info = maybe_validator_stake_info.unwrap();
        check_validator_stake_address(
            program_id,
            stake_pool_info.key,
            validator_stake_account_info.key,
            &vote_account_address,
            NonZeroU32::new(validator_stake_info.validator_seed_suffix),
        )?;
        if validator_stake_info.transient_stake_lamports > 0 {
            return Err(StakePoolError::TransientAccountInUse.into());
        }

        let transient_stake_bump_seed = check_transient_stake_address(
            program_id,
            stake_pool_info.key,
            transient_stake_account_info.key,
            &vote_account_address,
            transient_stake_seed,
        )?;
        let transient_stake_account_signer_seeds: &[&[_]] = &[
            TRANSIENT_STAKE_SEED_PREFIX,
            &vote_account_address.to_bytes(),
            &stake_pool_info.key.to_bytes(),
            &transient_stake_seed.to_le_bytes(),
            &[transient_stake_bump_seed],
        ];

        let stake_minimum_delegation = stake::tools::get_minimum_delegation()?;
        let stake_rent = rent.minimum_balance(std::mem::size_of::<stake::state::StakeState>());
        let current_minimum_lamports =
            stake_rent.saturating_add(minimum_delegation(stake_minimum_delegation));
        if lamports < current_minimum_lamports {
            msg!(
                "Need at least {} lamports for transient stake meet minimum delegation and rent-exempt requirements, {} provided",
                current_minimum_lamports,
                lamports
            );
            return Err(ProgramError::AccountNotRentExempt);
        }

        let remaining_lamports = validator_stake_account_info
            .lamports()
            .checked_sub(lamports)
            .ok_or(ProgramError::InsufficientFunds)?;
        let required_lamports = minimum_stake_lamports(&meta, stake_minimum_delegation);
        if remaining_lamports < required_lamports {
            msg!("Need at least {} lamports in the stake account after decrease, {} requested, {} is the current possible maximum",
                required_lamports,
                lamports,
                validator_stake_account_info.lamports().checked_sub(required_lamports).ok_or(StakePoolError::CalculationFailure)?
            );
            return Err(ProgramError::InsufficientFunds);
        }

        create_stake_account(
            transient_stake_account_info.clone(),
            transient_stake_account_signer_seeds,
            system_program_info.clone(),
        )?;

        // split into transient stake account
        Self::stake_split(
            stake_pool_info.key,
            validator_stake_account_info.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            lamports,
            transient_stake_account_info.clone(),
        )?;

        // deactivate transient stake
        Self::stake_deactivate(
            transient_stake_account_info.clone(),
            clock_info.clone(),
            withdraw_authority_info.clone(),
            stake_pool_info.key,
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
        )?;

        validator_stake_info.active_stake_lamports = validator_stake_info
            .active_stake_lamports
            .checked_sub(lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        validator_stake_info.transient_stake_lamports = lamports;
        validator_stake_info.transient_seed_suffix = transient_stake_seed;

        Ok(())
    }

    /// Processes `IncreaseValidatorStake` instruction.
    #[inline(never)] // needed due to stack size violation
    fn process_increase_validator_stake(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        lamports: u64,
        transient_stake_seed: u64,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let staker_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let reserve_stake_account_info = next_account_info(account_info_iter)?;
        let transient_stake_account_info = next_account_info(account_info_iter)?;
        let validator_stake_account_info = next_account_info(account_info_iter)?;
        let validator_vote_account_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let rent_info = next_account_info(account_info_iter)?;
        let rent = &Rent::from_account_info(rent_info)?;
        let stake_history_info = next_account_info(account_info_iter)?;
        let stake_config_info = next_account_info(account_info_iter)?;
        let system_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_system_program(system_program_info.key)?;
        check_stake_program(stake_program_info.key)?;
        check_account_owner(stake_pool_info, program_id)?;

        let stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            msg!("Expected valid stake pool");
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_staker(staker_info)?;

        if stake_pool.last_update_epoch < clock.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        stake_pool.check_validator_list(validator_list_info)?;
        stake_pool.check_reserve_stake(reserve_stake_account_info)?;
        check_account_owner(validator_list_info, program_id)?;

        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, mut validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let vote_account_address = validator_vote_account_info.key;

        let maybe_validator_stake_info = validator_list.find_mut::<ValidatorStakeInfo, _>(|x| {
            ValidatorStakeInfo::memcmp_pubkey(x, vote_account_address)
        });
        if maybe_validator_stake_info.is_none() {
            msg!(
                "Vote account {} not found in stake pool",
                vote_account_address
            );
            return Err(StakePoolError::ValidatorNotFound.into());
        }
        let mut validator_stake_info = maybe_validator_stake_info.unwrap();
        if validator_stake_info.transient_stake_lamports > 0 {
            return Err(StakePoolError::TransientAccountInUse.into());
        }

        // Check that the validator stake account is actually delegated to the right
        // validator. This can happen if a validator was force destaked during a
        // cluster restart.
        {
            check_account_owner(validator_stake_account_info, stake_program_info.key)?;
            check_validator_stake_address(
                program_id,
                stake_pool_info.key,
                validator_stake_account_info.key,
                vote_account_address,
                NonZeroU32::new(validator_stake_info.validator_seed_suffix),
            )?;
            let (meta, stake) = get_stake_state(validator_stake_account_info)?;
            if !stake_is_usable_by_pool(&meta, withdraw_authority_info.key, &stake_pool.lockup) {
                msg!("Validator stake for {} not usable by pool, must be owned by withdraw authority", vote_account_address);
                return Err(StakePoolError::WrongStakeState.into());
            }
            if stake.delegation.voter_pubkey != *vote_account_address {
                msg!(
                    "Validator stake {} not delegated to {}",
                    validator_stake_account_info.key,
                    vote_account_address
                );
                return Err(StakePoolError::WrongStakeState.into());
            }
        }

        let transient_stake_bump_seed = check_transient_stake_address(
            program_id,
            stake_pool_info.key,
            transient_stake_account_info.key,
            vote_account_address,
            transient_stake_seed,
        )?;
        let transient_stake_account_signer_seeds: &[&[_]] = &[
            TRANSIENT_STAKE_SEED_PREFIX,
            &vote_account_address.to_bytes(),
            &stake_pool_info.key.to_bytes(),
            &transient_stake_seed.to_le_bytes(),
            &[transient_stake_bump_seed],
        ];

        if validator_stake_info.status != StakeStatus::Active {
            msg!("Validator is marked for removal and no longer allows increases");
            return Err(StakePoolError::ValidatorNotFound.into());
        }

        let stake_rent = rent.minimum_balance(std::mem::size_of::<stake::state::StakeState>());
        let stake_minimum_delegation = stake::tools::get_minimum_delegation()?;
        let current_minimum_delegation = minimum_delegation(stake_minimum_delegation);
        if lamports < current_minimum_delegation {
            msg!(
                "Need more than {} lamports for transient stake to meet minimum delegation requirement, {} provided",
                current_minimum_delegation,
                lamports
            );
            return Err(ProgramError::Custom(
                stake::instruction::StakeError::InsufficientDelegation as u32,
            ));
        }

        // the stake account rent exemption is withdrawn after the merge, so
        let total_lamports = lamports.saturating_add(stake_rent);

        if reserve_stake_account_info
            .lamports()
            .saturating_sub(total_lamports)
            <= stake_rent
        {
            let max_split_amount = reserve_stake_account_info
                .lamports()
                .saturating_sub(2 * stake_rent);
            msg!(
                "Reserve stake does not have enough lamports for increase, must be less than {}, {} requested",
                max_split_amount,
                lamports
            );
            return Err(ProgramError::InsufficientFunds);
        }

        create_stake_account(
            transient_stake_account_info.clone(),
            transient_stake_account_signer_seeds,
            system_program_info.clone(),
        )?;

        // split into transient stake account
        Self::stake_split(
            stake_pool_info.key,
            reserve_stake_account_info.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            total_lamports,
            transient_stake_account_info.clone(),
        )?;

        // activate transient stake to validator
        Self::stake_delegate(
            transient_stake_account_info.clone(),
            validator_vote_account_info.clone(),
            clock_info.clone(),
            stake_history_info.clone(),
            stake_config_info.clone(),
            withdraw_authority_info.clone(),
            stake_pool_info.key,
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
        )?;

        validator_stake_info.transient_stake_lamports = total_lamports;
        validator_stake_info.transient_seed_suffix = transient_stake_seed;

        Ok(())
    }

    /// Process `SetPreferredValidator` instruction
    #[inline(never)] // needed due to stack size violation
    fn process_set_preferred_validator(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        validator_type: PreferredValidatorType,
        vote_account_address: Option<Pubkey>,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let staker_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;

        check_account_owner(stake_pool_info, program_id)?;
        check_account_owner(validator_list_info, program_id)?;

        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            msg!("Expected valid stake pool");
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_staker(staker_info)?;
        stake_pool.check_validator_list(validator_list_info)?;

        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        if let Some(vote_account_address) = vote_account_address {
            let maybe_validator_stake_info = validator_list.find::<ValidatorStakeInfo, _>(|x| {
                ValidatorStakeInfo::memcmp_pubkey(x, &vote_account_address)
            });
            match maybe_validator_stake_info {
                Some(vsi) => {
                    if vsi.status != StakeStatus::Active {
                        msg!("Validator for {:?} about to be removed, cannot set as preferred deposit account", validator_type);
                        return Err(StakePoolError::InvalidPreferredValidator.into());
                    }
                }
                None => {
                    msg!("Validator for {:?} not present in the stake pool, cannot set as preferred deposit account", validator_type);
                    return Err(StakePoolError::ValidatorNotFound.into());
                }
            }
        }

        match validator_type {
            PreferredValidatorType::Deposit => {
                stake_pool.preferred_deposit_validator_vote_address = vote_account_address
            }
            PreferredValidatorType::Withdraw => {
                stake_pool.preferred_withdraw_validator_vote_address = vote_account_address
            }
        };
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;
        Ok(())
    }

    /// Processes `UpdateValidatorListBalance` instruction.
    #[inline(always)] // needed to maximize number of validators
    fn process_update_validator_list_balance(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        start_index: u32,
        no_merge: bool,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let reserve_stake_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let stake_history_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;
        let validator_stake_accounts = account_info_iter.as_slice();

        check_account_owner(stake_pool_info, program_id)?;
        let stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }
        stake_pool.check_validator_list(validator_list_info)?;
        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_reserve_stake(reserve_stake_info)?;
        check_stake_program(stake_program_info.key)?;

        if validator_stake_accounts
            .len()
            .checked_rem(2)
            .ok_or(StakePoolError::CalculationFailure)?
            != 0
        {
            msg!("Odd number of validator stake accounts passed in, should be pairs of validator stake and transient stake accounts");
            return Err(StakePoolError::UnexpectedValidatorListAccountSize.into());
        }

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (validator_list_header, mut validator_slice) =
            ValidatorListHeader::deserialize_mut_slice(
                &mut validator_list_data,
                start_index as usize,
                validator_stake_accounts.len() / 2,
            )?;

        if !validator_list_header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let validator_iter = &mut validator_slice
            .iter_mut()
            .zip(validator_stake_accounts.chunks_exact(2));
        for (validator_stake_record, validator_stakes) in validator_iter {
            // chunks_exact means that we always get 2 elements, making this safe
            let validator_stake_info = validator_stakes.first().unwrap();
            let transient_stake_info = validator_stakes.last().unwrap();
            if check_validator_stake_address(
                program_id,
                stake_pool_info.key,
                validator_stake_info.key,
                &validator_stake_record.vote_account_address,
                NonZeroU32::new(validator_stake_record.validator_seed_suffix),
            )
            .is_err()
            {
                continue;
            };
            if check_transient_stake_address(
                program_id,
                stake_pool_info.key,
                transient_stake_info.key,
                &validator_stake_record.vote_account_address,
                validator_stake_record.transient_seed_suffix,
            )
            .is_err()
            {
                continue;
            };

            let mut active_stake_lamports = 0;
            let mut transient_stake_lamports = 0;
            let validator_stake_state = try_from_slice_unchecked::<stake::state::StakeState>(
                &validator_stake_info.data.borrow(),
            )
            .ok();
            let transient_stake_state = try_from_slice_unchecked::<stake::state::StakeState>(
                &transient_stake_info.data.borrow(),
            )
            .ok();

            // Possible merge situations for transient stake
            //  * active -> merge into validator stake
            //  * activating -> nothing, just account its lamports
            //  * deactivating -> nothing, just account its lamports
            //  * inactive -> merge into reserve stake
            //  * not a stake -> ignore
            match transient_stake_state {
                Some(stake::state::StakeState::Initialized(meta)) => {
                    if stake_is_usable_by_pool(
                        &meta,
                        withdraw_authority_info.key,
                        &stake_pool.lockup,
                    ) {
                        if no_merge {
                            transient_stake_lamports = transient_stake_info.lamports();
                        } else {
                            // merge into reserve
                            Self::stake_merge(
                                stake_pool_info.key,
                                transient_stake_info.clone(),
                                withdraw_authority_info.clone(),
                                AUTHORITY_WITHDRAW,
                                stake_pool.stake_withdraw_bump_seed,
                                reserve_stake_info.clone(),
                                clock_info.clone(),
                                stake_history_info.clone(),
                                stake_program_info.clone(),
                            )?;
                            validator_stake_record.status.remove_transient_stake();
                        }
                    }
                }
                Some(stake::state::StakeState::Stake(meta, stake)) => {
                    if stake_is_usable_by_pool(
                        &meta,
                        withdraw_authority_info.key,
                        &stake_pool.lockup,
                    ) {
                        if no_merge {
                            transient_stake_lamports = transient_stake_info.lamports();
                        } else if stake_is_inactive_without_history(&stake, clock.epoch) {
                            // deactivated, merge into reserve
                            Self::stake_merge(
                                stake_pool_info.key,
                                transient_stake_info.clone(),
                                withdraw_authority_info.clone(),
                                AUTHORITY_WITHDRAW,
                                stake_pool.stake_withdraw_bump_seed,
                                reserve_stake_info.clone(),
                                clock_info.clone(),
                                stake_history_info.clone(),
                                stake_program_info.clone(),
                            )?;
                            validator_stake_record.status.remove_transient_stake();
                        } else if stake.delegation.activation_epoch < clock.epoch {
                            if let Some(stake::state::StakeState::Stake(_, validator_stake)) =
                                validator_stake_state
                            {
                                if validator_stake.delegation.activation_epoch < clock.epoch {
                                    Self::stake_merge(
                                        stake_pool_info.key,
                                        transient_stake_info.clone(),
                                        withdraw_authority_info.clone(),
                                        AUTHORITY_WITHDRAW,
                                        stake_pool.stake_withdraw_bump_seed,
                                        validator_stake_info.clone(),
                                        clock_info.clone(),
                                        stake_history_info.clone(),
                                        stake_program_info.clone(),
                                    )?;
                                } else {
                                    msg!("Stake activating or just active, not ready to merge");
                                    transient_stake_lamports = transient_stake_info.lamports();
                                }
                            } else {
                                msg!("Transient stake is activating or active, but validator stake is not, need to add the validator stake account on {} back into the stake pool", stake.delegation.voter_pubkey);
                                transient_stake_lamports = transient_stake_info.lamports();
                            }
                        } else {
                            msg!("Transient stake not ready to be merged anywhere");
                            transient_stake_lamports = transient_stake_info.lamports();
                        }
                    }
                }
                None
                | Some(stake::state::StakeState::Uninitialized)
                | Some(stake::state::StakeState::RewardsPool) => {} // do nothing
            }

            // Status for validator stake
            //  * active -> do everything
            //  * any other state / not a stake -> error state, but account for transient stake
            let validator_stake_state = try_from_slice_unchecked::<stake::state::StakeState>(
                &validator_stake_info.data.borrow(),
            )
            .ok();
            match validator_stake_state {
                Some(stake::state::StakeState::Stake(meta, stake)) => {
                    let additional_lamports = validator_stake_info
                        .lamports()
                        .saturating_sub(stake.delegation.stake)
                        .saturating_sub(meta.rent_exempt_reserve);
                    // withdraw any extra lamports back to the reserve
                    if additional_lamports > 0
                        && stake_is_usable_by_pool(
                            &meta,
                            withdraw_authority_info.key,
                            &stake_pool.lockup,
                        )
                    {
                        Self::stake_withdraw(
                            stake_pool_info.key,
                            validator_stake_info.clone(),
                            withdraw_authority_info.clone(),
                            AUTHORITY_WITHDRAW,
                            stake_pool.stake_withdraw_bump_seed,
                            reserve_stake_info.clone(),
                            clock_info.clone(),
                            stake_history_info.clone(),
                            stake_program_info.clone(),
                            additional_lamports,
                        )?;
                    }
                    match validator_stake_record.status {
                        StakeStatus::Active => {
                            active_stake_lamports = validator_stake_info.lamports();
                        }
                        StakeStatus::DeactivatingValidator | StakeStatus::DeactivatingAll => {
                            if no_merge {
                                active_stake_lamports = validator_stake_info.lamports();
                            } else if stake_is_usable_by_pool(
                                &meta,
                                withdraw_authority_info.key,
                                &stake_pool.lockup,
                            ) && stake_is_inactive_without_history(&stake, clock.epoch)
                            {
                                // Validator was removed through normal means.
                                // Absorb the lamports into the reserve.
                                Self::stake_merge(
                                    stake_pool_info.key,
                                    validator_stake_info.clone(),
                                    withdraw_authority_info.clone(),
                                    AUTHORITY_WITHDRAW,
                                    stake_pool.stake_withdraw_bump_seed,
                                    reserve_stake_info.clone(),
                                    clock_info.clone(),
                                    stake_history_info.clone(),
                                    stake_program_info.clone(),
                                )?;
                                validator_stake_record.status.remove_validator_stake();
                            }
                        }
                        StakeStatus::DeactivatingTransient | StakeStatus::ReadyForRemoval => {
                            msg!("Validator stake account no longer part of the pool, ignoring");
                        }
                    }
                }
                Some(stake::state::StakeState::Initialized(meta))
                    if stake_is_usable_by_pool(
                        &meta,
                        withdraw_authority_info.key,
                        &stake_pool.lockup,
                    ) =>
                {
                    // If a validator stake is `Initialized`, the validator could
                    // have been destaked during a cluster restart or removed through
                    // normal means. Either way, absorb those lamports into the reserve.
                    // The transient stake was likely absorbed into the reserve earlier.
                    Self::stake_merge(
                        stake_pool_info.key,
                        validator_stake_info.clone(),
                        withdraw_authority_info.clone(),
                        AUTHORITY_WITHDRAW,
                        stake_pool.stake_withdraw_bump_seed,
                        reserve_stake_info.clone(),
                        clock_info.clone(),
                        stake_history_info.clone(),
                        stake_program_info.clone(),
                    )?;
                    validator_stake_record.status.remove_validator_stake();
                }
                Some(stake::state::StakeState::Initialized(_))
                | Some(stake::state::StakeState::Uninitialized)
                | Some(stake::state::StakeState::RewardsPool)
                | None => {
                    msg!("Validator stake account no longer part of the pool, ignoring");
                }
            }

            validator_stake_record.last_update_epoch = clock.epoch;
            validator_stake_record.active_stake_lamports = active_stake_lamports;
            validator_stake_record.transient_stake_lamports = transient_stake_lamports;
        }

        Ok(())
    }

    /// Processes `UpdateStakePoolBalance` instruction.
    #[inline(always)] // needed to optimize number of validators
    fn process_update_stake_pool_balance(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let withdraw_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let reserve_stake_info = next_account_info(account_info_iter)?;
        let manager_fee_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let clock = Clock::get()?;

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }
        stake_pool.check_mint(pool_mint_info)?;
        stake_pool.check_authority_withdraw(withdraw_info.key, program_id, stake_pool_info.key)?;
        stake_pool.check_reserve_stake(reserve_stake_info)?;
        if stake_pool.manager_fee_account != *manager_fee_info.key {
            return Err(StakePoolError::InvalidFeeAccount.into());
        }

        if *validator_list_info.key != stake_pool.validator_list {
            return Err(StakePoolError::InvalidValidatorStakeList.into());
        }
        if stake_pool.token_program_id != *token_program_info.key {
            return Err(ProgramError::IncorrectProgramId);
        }

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let previous_lamports = stake_pool.total_lamports;
        let previous_pool_token_supply = stake_pool.pool_token_supply;
        let reserve_stake = try_from_slice_unchecked::<stake::state::StakeState>(
            &reserve_stake_info.data.borrow(),
        )?;
        let mut total_lamports = if let stake::state::StakeState::Initialized(meta) = reserve_stake
        {
            reserve_stake_info
                .lamports()
                .checked_sub(minimum_reserve_lamports(&meta))
                .ok_or(StakePoolError::CalculationFailure)?
        } else {
            msg!("Reserve stake account in unknown state, aborting");
            return Err(StakePoolError::WrongStakeState.into());
        };
        for validator_stake_record in validator_list.iter::<ValidatorStakeInfo>() {
            if validator_stake_record.last_update_epoch < clock.epoch {
                return Err(StakePoolError::StakeListOutOfDate.into());
            }
            total_lamports = total_lamports
                .checked_add(validator_stake_record.stake_lamports())
                .ok_or(StakePoolError::CalculationFailure)?;
        }

        let reward_lamports = total_lamports.saturating_sub(previous_lamports);

        // If the manager fee info is invalid, they don't deserve to receive the fee.
        let fee = if stake_pool.check_manager_fee_info(manager_fee_info).is_ok() {
            stake_pool
                .calc_epoch_fee_amount(reward_lamports)
                .ok_or(StakePoolError::CalculationFailure)?
        } else {
            0
        };

        if fee > 0 {
            Self::token_mint_to(
                stake_pool_info.key,
                token_program_info.clone(),
                pool_mint_info.clone(),
                manager_fee_info.clone(),
                withdraw_info.clone(),
                AUTHORITY_WITHDRAW,
                stake_pool.stake_withdraw_bump_seed,
                fee,
            )?;
        }

        if stake_pool.last_update_epoch < clock.epoch {
            if let Some(fee) = stake_pool.next_epoch_fee {
                stake_pool.epoch_fee = fee;
                stake_pool.next_epoch_fee = None;
            }
            if let Some(fee) = stake_pool.next_stake_withdrawal_fee {
                stake_pool.stake_withdrawal_fee = fee;
                stake_pool.next_stake_withdrawal_fee = None;
            }
            if let Some(fee) = stake_pool.next_sol_withdrawal_fee {
                stake_pool.sol_withdrawal_fee = fee;
                stake_pool.next_sol_withdrawal_fee = None;
            }
            stake_pool.last_update_epoch = clock.epoch;
            stake_pool.last_epoch_total_lamports = previous_lamports;
            stake_pool.last_epoch_pool_token_supply = previous_pool_token_supply;
        }
        stake_pool.total_lamports = total_lamports;

        let pool_mint_data = pool_mint_info.try_borrow_data()?;
        let pool_mint = StateWithExtensions::<Mint>::unpack(&pool_mint_data)?;
        stake_pool.pool_token_supply = pool_mint.base.supply;

        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes the `CleanupRemovedValidatorEntries` instruction
    #[inline(never)] // needed to avoid stack size violation
    fn process_cleanup_removed_validator_entries(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;

        check_account_owner(stake_pool_info, program_id)?;
        let stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }
        stake_pool.check_validator_list(validator_list_info)?;

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, mut validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        validator_list.retain::<ValidatorStakeInfo>(ValidatorStakeInfo::is_not_removed)?;

        Ok(())
    }

    /// Processes [DepositStake](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_deposit_stake(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let stake_deposit_authority_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let stake_info = next_account_info(account_info_iter)?;
        let validator_stake_account_info = next_account_info(account_info_iter)?;
        let reserve_stake_account_info = next_account_info(account_info_iter)?;
        let dest_user_pool_info = next_account_info(account_info_iter)?;
        let manager_fee_info = next_account_info(account_info_iter)?;
        let referrer_fee_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let stake_history_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_stake_program(stake_program_info.key)?;

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_stake_deposit_authority(stake_deposit_authority_info.key)?;
        stake_pool.check_mint(pool_mint_info)?;
        stake_pool.check_validator_list(validator_list_info)?;
        stake_pool.check_reserve_stake(reserve_stake_account_info)?;

        if stake_pool.token_program_id != *token_program_info.key {
            return Err(ProgramError::IncorrectProgramId);
        }

        if stake_pool.manager_fee_account != *manager_fee_info.key {
            return Err(StakePoolError::InvalidFeeAccount.into());
        }
        // There is no bypass if the manager fee account is invalid. Deposits
        // don't hold user funds hostage, so if the fee account is invalid, users
        // cannot deposit in the pool.  Let it fail here!

        if stake_pool.last_update_epoch < clock.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, mut validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let (_, validator_stake) = get_stake_state(validator_stake_account_info)?;
        let pre_all_validator_lamports = validator_stake_account_info.lamports();
        let vote_account_address = validator_stake.delegation.voter_pubkey;
        if let Some(preferred_deposit) = stake_pool.preferred_deposit_validator_vote_address {
            if preferred_deposit != vote_account_address {
                msg!(
                    "Incorrect deposit address, expected {}, received {}",
                    preferred_deposit,
                    vote_account_address
                );
                return Err(StakePoolError::IncorrectDepositVoteAddress.into());
            }
        }

        let mut validator_stake_info = validator_list
            .find_mut::<ValidatorStakeInfo, _>(|x| {
                ValidatorStakeInfo::memcmp_pubkey(x, &vote_account_address)
            })
            .ok_or(StakePoolError::ValidatorNotFound)?;
        check_validator_stake_address(
            program_id,
            stake_pool_info.key,
            validator_stake_account_info.key,
            &vote_account_address,
            NonZeroU32::new(validator_stake_info.validator_seed_suffix),
        )?;

        if validator_stake_info.status != StakeStatus::Active {
            msg!("Validator is marked for removal and no longer accepting deposits");
            return Err(StakePoolError::ValidatorNotFound.into());
        }

        msg!("Stake pre merge {}", validator_stake.delegation.stake);

        let (stake_deposit_authority_program_address, deposit_bump_seed) =
            find_deposit_authority_program_address(program_id, stake_pool_info.key);
        if *stake_deposit_authority_info.key == stake_deposit_authority_program_address {
            Self::stake_authorize_signed(
                stake_pool_info.key,
                stake_info.clone(),
                stake_deposit_authority_info.clone(),
                AUTHORITY_DEPOSIT,
                deposit_bump_seed,
                withdraw_authority_info.key,
                clock_info.clone(),
                stake_program_info.clone(),
            )?;
        } else {
            Self::stake_authorize(
                stake_info.clone(),
                stake_deposit_authority_info.clone(),
                withdraw_authority_info.key,
                clock_info.clone(),
                stake_program_info.clone(),
            )?;
        }

        Self::stake_merge(
            stake_pool_info.key,
            stake_info.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            validator_stake_account_info.clone(),
            clock_info.clone(),
            stake_history_info.clone(),
            stake_program_info.clone(),
        )?;

        let (_, post_validator_stake) = get_stake_state(validator_stake_account_info)?;
        let post_all_validator_lamports = validator_stake_account_info.lamports();
        msg!("Stake post merge {}", post_validator_stake.delegation.stake);

        let total_deposit_lamports = post_all_validator_lamports
            .checked_sub(pre_all_validator_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        let stake_deposit_lamports = post_validator_stake
            .delegation
            .stake
            .checked_sub(validator_stake.delegation.stake)
            .ok_or(StakePoolError::CalculationFailure)?;
        let sol_deposit_lamports = total_deposit_lamports
            .checked_sub(stake_deposit_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;

        let new_pool_tokens = stake_pool
            .calc_pool_tokens_for_deposit(total_deposit_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        let new_pool_tokens_from_stake = stake_pool
            .calc_pool_tokens_for_deposit(stake_deposit_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        let new_pool_tokens_from_sol = new_pool_tokens
            .checked_sub(new_pool_tokens_from_stake)
            .ok_or(StakePoolError::CalculationFailure)?;

        let stake_deposit_fee = stake_pool
            .calc_pool_tokens_stake_deposit_fee(new_pool_tokens_from_stake)
            .ok_or(StakePoolError::CalculationFailure)?;
        let sol_deposit_fee = stake_pool
            .calc_pool_tokens_sol_deposit_fee(new_pool_tokens_from_sol)
            .ok_or(StakePoolError::CalculationFailure)?;

        let total_fee = stake_deposit_fee
            .checked_add(sol_deposit_fee)
            .ok_or(StakePoolError::CalculationFailure)?;
        let pool_tokens_user = new_pool_tokens
            .checked_sub(total_fee)
            .ok_or(StakePoolError::CalculationFailure)?;

        let pool_tokens_referral_fee = stake_pool
            .calc_pool_tokens_stake_referral_fee(total_fee)
            .ok_or(StakePoolError::CalculationFailure)?;

        let pool_tokens_manager_deposit_fee = total_fee
            .checked_sub(pool_tokens_referral_fee)
            .ok_or(StakePoolError::CalculationFailure)?;

        if pool_tokens_user
            .saturating_add(pool_tokens_manager_deposit_fee)
            .saturating_add(pool_tokens_referral_fee)
            != new_pool_tokens
        {
            return Err(StakePoolError::CalculationFailure.into());
        }

        if pool_tokens_user == 0 {
            return Err(StakePoolError::DepositTooSmall.into());
        }

        Self::token_mint_to(
            stake_pool_info.key,
            token_program_info.clone(),
            pool_mint_info.clone(),
            dest_user_pool_info.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            pool_tokens_user,
        )?;
        if pool_tokens_manager_deposit_fee > 0 {
            Self::token_mint_to(
                stake_pool_info.key,
                token_program_info.clone(),
                pool_mint_info.clone(),
                manager_fee_info.clone(),
                withdraw_authority_info.clone(),
                AUTHORITY_WITHDRAW,
                stake_pool.stake_withdraw_bump_seed,
                pool_tokens_manager_deposit_fee,
            )?;
        }
        if pool_tokens_referral_fee > 0 {
            Self::token_mint_to(
                stake_pool_info.key,
                token_program_info.clone(),
                pool_mint_info.clone(),
                referrer_fee_info.clone(),
                withdraw_authority_info.clone(),
                AUTHORITY_WITHDRAW,
                stake_pool.stake_withdraw_bump_seed,
                pool_tokens_referral_fee,
            )?;
        }

        // withdraw additional lamports to the reserve
        if sol_deposit_lamports > 0 {
            Self::stake_withdraw(
                stake_pool_info.key,
                validator_stake_account_info.clone(),
                withdraw_authority_info.clone(),
                AUTHORITY_WITHDRAW,
                stake_pool.stake_withdraw_bump_seed,
                reserve_stake_account_info.clone(),
                clock_info.clone(),
                stake_history_info.clone(),
                stake_program_info.clone(),
                sol_deposit_lamports,
            )?;
        }

        stake_pool.pool_token_supply = stake_pool
            .pool_token_supply
            .checked_add(new_pool_tokens)
            .ok_or(StakePoolError::CalculationFailure)?;
        // We treat the extra lamports as though they were
        // transferred directly to the reserve stake account.
        stake_pool.total_lamports = stake_pool
            .total_lamports
            .checked_add(total_deposit_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;

        validator_stake_info.active_stake_lamports = validator_stake_account_info.lamports();

        Ok(())
    }

    /// Processes [DepositSol](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_deposit_sol(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        deposit_lamports: u64,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let reserve_stake_account_info = next_account_info(account_info_iter)?;
        let from_user_lamports_info = next_account_info(account_info_iter)?;
        let dest_user_pool_info = next_account_info(account_info_iter)?;
        let manager_fee_info = next_account_info(account_info_iter)?;
        let referrer_fee_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let system_program_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let sol_deposit_authority_info = next_account_info(account_info_iter);

        let clock = Clock::get()?;

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_sol_deposit_authority(sol_deposit_authority_info)?;
        stake_pool.check_mint(pool_mint_info)?;
        stake_pool.check_reserve_stake(reserve_stake_account_info)?;

        if stake_pool.token_program_id != *token_program_info.key {
            return Err(ProgramError::IncorrectProgramId);
        }
        check_system_program(system_program_info.key)?;

        if stake_pool.manager_fee_account != *manager_fee_info.key {
            return Err(StakePoolError::InvalidFeeAccount.into());
        }
        // There is no bypass if the manager fee account is invalid. Deposits
        // don't hold user funds hostage, so if the fee account is invalid, users
        // cannot deposit in the pool.  Let it fail here!

        // We want this to hold to ensure that deposit_sol mints pool tokens
        // at the right price
        if stake_pool.last_update_epoch < clock.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        let new_pool_tokens = stake_pool
            .calc_pool_tokens_for_deposit(deposit_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;

        let pool_tokens_sol_deposit_fee = stake_pool
            .calc_pool_tokens_sol_deposit_fee(new_pool_tokens)
            .ok_or(StakePoolError::CalculationFailure)?;
        let pool_tokens_user = new_pool_tokens
            .checked_sub(pool_tokens_sol_deposit_fee)
            .ok_or(StakePoolError::CalculationFailure)?;

        let pool_tokens_referral_fee = stake_pool
            .calc_pool_tokens_sol_referral_fee(pool_tokens_sol_deposit_fee)
            .ok_or(StakePoolError::CalculationFailure)?;
        let pool_tokens_manager_deposit_fee = pool_tokens_sol_deposit_fee
            .checked_sub(pool_tokens_referral_fee)
            .ok_or(StakePoolError::CalculationFailure)?;

        if pool_tokens_user
            .saturating_add(pool_tokens_manager_deposit_fee)
            .saturating_add(pool_tokens_referral_fee)
            != new_pool_tokens
        {
            return Err(StakePoolError::CalculationFailure.into());
        }

        if pool_tokens_user == 0 {
            return Err(StakePoolError::DepositTooSmall.into());
        }

        Self::sol_transfer(
            from_user_lamports_info.clone(),
            reserve_stake_account_info.clone(),
            system_program_info.clone(),
            deposit_lamports,
        )?;

        Self::token_mint_to(
            stake_pool_info.key,
            token_program_info.clone(),
            pool_mint_info.clone(),
            dest_user_pool_info.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            pool_tokens_user,
        )?;

        if pool_tokens_manager_deposit_fee > 0 {
            Self::token_mint_to(
                stake_pool_info.key,
                token_program_info.clone(),
                pool_mint_info.clone(),
                manager_fee_info.clone(),
                withdraw_authority_info.clone(),
                AUTHORITY_WITHDRAW,
                stake_pool.stake_withdraw_bump_seed,
                pool_tokens_manager_deposit_fee,
            )?;
        }

        if pool_tokens_referral_fee > 0 {
            Self::token_mint_to(
                stake_pool_info.key,
                token_program_info.clone(),
                pool_mint_info.clone(),
                referrer_fee_info.clone(),
                withdraw_authority_info.clone(),
                AUTHORITY_WITHDRAW,
                stake_pool.stake_withdraw_bump_seed,
                pool_tokens_referral_fee,
            )?;
        }

        stake_pool.pool_token_supply = stake_pool
            .pool_token_supply
            .checked_add(new_pool_tokens)
            .ok_or(StakePoolError::CalculationFailure)?;
        stake_pool.total_lamports = stake_pool
            .total_lamports
            .checked_add(deposit_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes [WithdrawStake](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_withdraw_stake(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        pool_tokens: u64,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let validator_list_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let stake_split_from = next_account_info(account_info_iter)?;
        let stake_split_to = next_account_info(account_info_iter)?;
        let user_stake_authority_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let burn_from_pool_info = next_account_info(account_info_iter)?;
        let manager_fee_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_stake_program(stake_program_info.key)?;
        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let decimals = stake_pool.check_mint(pool_mint_info)?;
        stake_pool.check_validator_list(validator_list_info)?;
        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;

        if stake_pool.manager_fee_account != *manager_fee_info.key {
            return Err(StakePoolError::InvalidFeeAccount.into());
        }
        if stake_pool.token_program_id != *token_program_info.key {
            return Err(ProgramError::IncorrectProgramId);
        }

        if stake_pool.last_update_epoch < clock.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        check_account_owner(validator_list_info, program_id)?;
        let mut validator_list_data = validator_list_info.data.borrow_mut();
        let (header, mut validator_list) =
            ValidatorListHeader::deserialize_vec(&mut validator_list_data)?;
        if !header.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        // To prevent a faulty manager fee account from preventing withdrawals
        // if the token program does not own the account, or if the account is not initialized
        let pool_tokens_fee = if stake_pool.manager_fee_account == *burn_from_pool_info.key
            || stake_pool.check_manager_fee_info(manager_fee_info).is_err()
        {
            0
        } else {
            stake_pool
                .calc_pool_tokens_stake_withdrawal_fee(pool_tokens)
                .ok_or(StakePoolError::CalculationFailure)?
        };
        let pool_tokens_burnt = pool_tokens
            .checked_sub(pool_tokens_fee)
            .ok_or(StakePoolError::CalculationFailure)?;

        let mut withdraw_lamports = stake_pool
            .calc_lamports_withdraw_amount(pool_tokens_burnt)
            .ok_or(StakePoolError::CalculationFailure)?;

        if withdraw_lamports == 0 {
            return Err(StakePoolError::WithdrawalTooSmall.into());
        }

        let stake_minimum_delegation = stake::tools::get_minimum_delegation()?;
        let stake_state =
            try_from_slice_unchecked::<stake::state::StakeState>(&stake_split_from.data.borrow())?;
        let meta = stake_state.meta().ok_or(StakePoolError::WrongStakeState)?;
        let required_lamports = minimum_stake_lamports(&meta, stake_minimum_delegation);

        let lamports_per_pool_token = stake_pool
            .get_lamports_per_pool_token()
            .ok_or(StakePoolError::CalculationFailure)?;
        let minimum_lamports_with_tolerance =
            required_lamports.saturating_add(lamports_per_pool_token);

        let has_active_stake = validator_list
            .find::<ValidatorStakeInfo, _>(|x| {
                ValidatorStakeInfo::active_lamports_greater_than(
                    x,
                    &minimum_lamports_with_tolerance,
                )
            })
            .is_some();
        let has_transient_stake = validator_list
            .find::<ValidatorStakeInfo, _>(|x| {
                ValidatorStakeInfo::transient_lamports_greater_than(
                    x,
                    &minimum_lamports_with_tolerance,
                )
            })
            .is_some();

        let validator_list_item_info = if *stake_split_from.key == stake_pool.reserve_stake {
            // check that the validator stake accounts have no withdrawable stake
            if has_transient_stake || has_active_stake {
                msg!("Error withdrawing from reserve: validator stake accounts have lamports available, please use those first.");
                return Err(StakePoolError::StakeLamportsNotEqualToMinimum.into());
            }

            // check that reserve has enough (should never fail, but who knows?)
            stake_split_from
                .lamports()
                .checked_sub(minimum_reserve_lamports(&meta))
                .ok_or(StakePoolError::StakeLamportsNotEqualToMinimum)?;
            None
        } else {
            let delegation = stake_state
                .delegation()
                .ok_or(StakePoolError::WrongStakeState)?;
            let vote_account_address = delegation.voter_pubkey;

            if let Some(preferred_withdraw_validator) =
                stake_pool.preferred_withdraw_validator_vote_address
            {
                let preferred_validator_info = validator_list
                    .find::<ValidatorStakeInfo, _>(|x| {
                        ValidatorStakeInfo::memcmp_pubkey(x, &preferred_withdraw_validator)
                    })
                    .ok_or(StakePoolError::ValidatorNotFound)?;
                let available_lamports = preferred_validator_info
                    .active_stake_lamports
                    .saturating_sub(minimum_lamports_with_tolerance);
                if preferred_withdraw_validator != vote_account_address && available_lamports > 0 {
                    msg!("Validator vote address {} is preferred for withdrawals, it currently has {} lamports available. Please withdraw those before using other validator stake accounts.", preferred_withdraw_validator, preferred_validator_info.active_stake_lamports);
                    return Err(StakePoolError::IncorrectWithdrawVoteAddress.into());
                }
            }

            let validator_stake_info = validator_list
                .find_mut::<ValidatorStakeInfo, _>(|x| {
                    ValidatorStakeInfo::memcmp_pubkey(x, &vote_account_address)
                })
                .ok_or(StakePoolError::ValidatorNotFound)?;

            let withdraw_source = if has_active_stake {
                // if there's any active stake, we must withdraw from an active
                // stake account
                check_validator_stake_address(
                    program_id,
                    stake_pool_info.key,
                    stake_split_from.key,
                    &vote_account_address,
                    NonZeroU32::new(validator_stake_info.validator_seed_suffix),
                )?;
                StakeWithdrawSource::Active
            } else if has_transient_stake {
                // if there's any transient stake, we must withdraw from there
                check_transient_stake_address(
                    program_id,
                    stake_pool_info.key,
                    stake_split_from.key,
                    &vote_account_address,
                    validator_stake_info.transient_seed_suffix,
                )?;
                StakeWithdrawSource::Transient
            } else {
                // if there's no active or transient stake, we can take the whole account
                check_validator_stake_address(
                    program_id,
                    stake_pool_info.key,
                    stake_split_from.key,
                    &vote_account_address,
                    NonZeroU32::new(validator_stake_info.validator_seed_suffix),
                )?;
                StakeWithdrawSource::ValidatorRemoval
            };

            if validator_stake_info.status != StakeStatus::Active {
                msg!("Validator is marked for removal and no longer allowing withdrawals");
                return Err(StakePoolError::ValidatorNotFound.into());
            }

            match withdraw_source {
                StakeWithdrawSource::Active | StakeWithdrawSource::Transient => {
                    let remaining_lamports = stake_split_from
                        .lamports()
                        .saturating_sub(withdraw_lamports);
                    if remaining_lamports < required_lamports {
                        msg!("Attempting to withdraw {} lamports from validator account with {} stake lamports, {} must remain", withdraw_lamports, stake_split_from.lamports(), required_lamports);
                        return Err(StakePoolError::StakeLamportsNotEqualToMinimum.into());
                    }
                }
                StakeWithdrawSource::ValidatorRemoval => {
                    let split_from_lamports = stake_split_from.lamports();
                    let upper_bound = split_from_lamports.saturating_add(lamports_per_pool_token);
                    if withdraw_lamports < split_from_lamports || withdraw_lamports > upper_bound {
                        msg!(
                            "Cannot withdraw a whole account worth {} lamports, \
                              must withdraw at least {} lamports worth of pool tokens \
                              with a margin of {} lamports",
                            withdraw_lamports,
                            split_from_lamports,
                            lamports_per_pool_token
                        );
                        return Err(StakePoolError::StakeLamportsNotEqualToMinimum.into());
                    }
                    // truncate the lamports down to the amount in the account
                    withdraw_lamports = split_from_lamports;
                }
            }
            Some((validator_stake_info, withdraw_source))
        };

        Self::token_burn(
            token_program_info.clone(),
            burn_from_pool_info.clone(),
            pool_mint_info.clone(),
            user_transfer_authority_info.clone(),
            pool_tokens_burnt,
        )?;

        Self::stake_split(
            stake_pool_info.key,
            stake_split_from.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            withdraw_lamports,
            stake_split_to.clone(),
        )?;

        Self::stake_authorize_signed(
            stake_pool_info.key,
            stake_split_to.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            user_stake_authority_info.key,
            clock_info.clone(),
            stake_program_info.clone(),
        )?;

        if pool_tokens_fee > 0 {
            Self::token_transfer(
                token_program_info.clone(),
                burn_from_pool_info.clone(),
                pool_mint_info.clone(),
                manager_fee_info.clone(),
                user_transfer_authority_info.clone(),
                pool_tokens_fee,
                decimals,
            )?;
        }

        stake_pool.pool_token_supply = stake_pool
            .pool_token_supply
            .checked_sub(pool_tokens_burnt)
            .ok_or(StakePoolError::CalculationFailure)?;
        stake_pool.total_lamports = stake_pool
            .total_lamports
            .checked_sub(withdraw_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;

        if let Some((validator_list_item, withdraw_source)) = validator_list_item_info {
            match withdraw_source {
                StakeWithdrawSource::Active => {
                    validator_list_item.active_stake_lamports = validator_list_item
                        .active_stake_lamports
                        .checked_sub(withdraw_lamports)
                        .ok_or(StakePoolError::CalculationFailure)?
                }
                StakeWithdrawSource::Transient => {
                    validator_list_item.transient_stake_lamports = validator_list_item
                        .transient_stake_lamports
                        .checked_sub(withdraw_lamports)
                        .ok_or(StakePoolError::CalculationFailure)?
                }
                StakeWithdrawSource::ValidatorRemoval => {
                    validator_list_item.active_stake_lamports = validator_list_item
                        .active_stake_lamports
                        .checked_sub(withdraw_lamports)
                        .ok_or(StakePoolError::CalculationFailure)?;
                    if validator_list_item.active_stake_lamports != 0 {
                        msg!("Attempting to remove a validator from the pool, but withdrawal leaves {} lamports, update the pool to merge any unaccounted lamports",
                            validator_list_item.active_stake_lamports);
                        return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
                    }
                    // since we already checked that there's no transient stake,
                    // we can immediately set this as ready for removal
                    validator_list_item.status = StakeStatus::ReadyForRemoval;
                }
            }
        }

        Ok(())
    }

    /// Processes [WithdrawSol](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_withdraw_sol(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        pool_tokens: u64,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let burn_from_pool_info = next_account_info(account_info_iter)?;
        let reserve_stake_info = next_account_info(account_info_iter)?;
        let destination_lamports_info = next_account_info(account_info_iter)?;
        let manager_fee_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let stake_history_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let sol_withdraw_authority_info = next_account_info(account_info_iter);

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_sol_withdraw_authority(sol_withdraw_authority_info)?;
        let decimals = stake_pool.check_mint(pool_mint_info)?;
        stake_pool.check_reserve_stake(reserve_stake_info)?;

        if stake_pool.token_program_id != *token_program_info.key {
            return Err(ProgramError::IncorrectProgramId);
        }
        check_stake_program(stake_program_info.key)?;

        if stake_pool.manager_fee_account != *manager_fee_info.key {
            return Err(StakePoolError::InvalidFeeAccount.into());
        }

        // We want this to hold to ensure that withdraw_sol burns pool tokens
        // at the right price
        if stake_pool.last_update_epoch < Clock::get()?.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        // To prevent a faulty manager fee account from preventing withdrawals
        // if the token program does not own the account, or if the account is not initialized
        let pool_tokens_fee = if stake_pool.manager_fee_account == *burn_from_pool_info.key
            || stake_pool.check_manager_fee_info(manager_fee_info).is_err()
        {
            0
        } else {
            stake_pool
                .calc_pool_tokens_sol_withdrawal_fee(pool_tokens)
                .ok_or(StakePoolError::CalculationFailure)?
        };
        let pool_tokens_burnt = pool_tokens
            .checked_sub(pool_tokens_fee)
            .ok_or(StakePoolError::CalculationFailure)?;

        let withdraw_lamports = stake_pool
            .calc_lamports_withdraw_amount(pool_tokens_burnt)
            .ok_or(StakePoolError::CalculationFailure)?;

        if withdraw_lamports == 0 {
            return Err(StakePoolError::WithdrawalTooSmall.into());
        }

        let new_reserve_lamports = reserve_stake_info
            .lamports()
            .saturating_sub(withdraw_lamports);
        let stake_state = try_from_slice_unchecked::<stake::state::StakeState>(
            &reserve_stake_info.data.borrow(),
        )?;
        if let stake::state::StakeState::Initialized(meta) = stake_state {
            let minimum_reserve_lamports = minimum_reserve_lamports(&meta);
            if new_reserve_lamports < minimum_reserve_lamports {
                msg!("Attempting to withdraw {} lamports, maximum possible SOL withdrawal is {} lamports",
                    withdraw_lamports,
                    reserve_stake_info.lamports().saturating_sub(minimum_reserve_lamports)
                );
                return Err(StakePoolError::SolWithdrawalTooLarge.into());
            }
        } else {
            msg!("Reserve stake account not in intialized state");
            return Err(StakePoolError::WrongStakeState.into());
        };

        Self::token_burn(
            token_program_info.clone(),
            burn_from_pool_info.clone(),
            pool_mint_info.clone(),
            user_transfer_authority_info.clone(),
            pool_tokens_burnt,
        )?;

        if pool_tokens_fee > 0 {
            Self::token_transfer(
                token_program_info.clone(),
                burn_from_pool_info.clone(),
                pool_mint_info.clone(),
                manager_fee_info.clone(),
                user_transfer_authority_info.clone(),
                pool_tokens_fee,
                decimals,
            )?;
        }

        Self::stake_withdraw(
            stake_pool_info.key,
            reserve_stake_info.clone(),
            withdraw_authority_info.clone(),
            AUTHORITY_WITHDRAW,
            stake_pool.stake_withdraw_bump_seed,
            destination_lamports_info.clone(),
            clock_info.clone(),
            stake_history_info.clone(),
            stake_program_info.clone(),
            withdraw_lamports,
        )?;

        stake_pool.pool_token_supply = stake_pool
            .pool_token_supply
            .checked_sub(pool_tokens_burnt)
            .ok_or(StakePoolError::CalculationFailure)?;
        stake_pool.total_lamports = stake_pool
            .total_lamports
            .checked_sub(withdraw_lamports)
            .ok_or(StakePoolError::CalculationFailure)?;
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;

        Ok(())
    }

    #[inline(never)]
    fn process_create_pool_token_metadata(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        name: String,
        symbol: String,
        uri: String,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let manager_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let payer_info = next_account_info(account_info_iter)?;
        let metadata_info = next_account_info(account_info_iter)?;
        let mpl_token_metadata_program_info = next_account_info(account_info_iter)?;
        let system_program_info = next_account_info(account_info_iter)?;
        let rent_sysvar_info = next_account_info(account_info_iter)?;

        if !payer_info.is_signer {
            msg!("Payer did not sign metadata creation");
            return Err(StakePoolError::SignatureMissing.into());
        }

        check_system_program(system_program_info.key)?;
        check_rent_sysvar(rent_sysvar_info.key)?;
        check_account_owner(payer_info, &system_program::id())?;
        check_account_owner(stake_pool_info, program_id)?;
        check_mpl_metadata_program(mpl_token_metadata_program_info.key)?;

        let stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_manager(manager_info)?;
        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        stake_pool.check_mint(pool_mint_info)?;
        check_mpl_metadata_account_address(metadata_info.key, &stake_pool.pool_mint)?;

        // Token mint authority for stake-pool token is stake-pool withdraw authority
        let token_mint_authority = withdraw_authority_info;

        let new_metadata_instruction = create_metadata_accounts_v3(
            *mpl_token_metadata_program_info.key,
            *metadata_info.key,
            *pool_mint_info.key,
            *token_mint_authority.key,
            *payer_info.key,
            *token_mint_authority.key,
            name,
            symbol,
            uri,
            None,
            0,
            true,
            true,
            None,
            None,
            None,
        );

        let (_, stake_withdraw_bump_seed) =
            crate::find_withdraw_authority_program_address(program_id, stake_pool_info.key);

        let token_mint_authority_signer_seeds: &[&[_]] = &[
            &stake_pool_info.key.to_bytes()[..32],
            AUTHORITY_WITHDRAW,
            &[stake_withdraw_bump_seed],
        ];

        invoke_signed(
            &new_metadata_instruction,
            &[
                metadata_info.clone(),
                pool_mint_info.clone(),
                withdraw_authority_info.clone(),
                payer_info.clone(),
                withdraw_authority_info.clone(),
                system_program_info.clone(),
                rent_sysvar_info.clone(),
                mpl_token_metadata_program_info.clone(),
            ],
            &[token_mint_authority_signer_seeds],
        )?;

        Ok(())
    }

    #[inline(never)]
    fn process_update_pool_token_metadata(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        name: String,
        symbol: String,
        uri: String,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();

        let stake_pool_info = next_account_info(account_info_iter)?;
        let manager_info = next_account_info(account_info_iter)?;
        let withdraw_authority_info = next_account_info(account_info_iter)?;
        let metadata_info = next_account_info(account_info_iter)?;
        let mpl_token_metadata_program_info = next_account_info(account_info_iter)?;

        check_account_owner(stake_pool_info, program_id)?;

        check_mpl_metadata_program(mpl_token_metadata_program_info.key)?;

        let stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_manager(manager_info)?;
        stake_pool.check_authority_withdraw(
            withdraw_authority_info.key,
            program_id,
            stake_pool_info.key,
        )?;
        check_mpl_metadata_account_address(metadata_info.key, &stake_pool.pool_mint)?;

        // Token mint authority for stake-pool token is withdraw authority only
        let token_mint_authority = withdraw_authority_info;

        let update_metadata_accounts_instruction = update_metadata_accounts_v2(
            *mpl_token_metadata_program_info.key,
            *metadata_info.key,
            *token_mint_authority.key,
            None,
            Some(DataV2 {
                name,
                symbol,
                uri,
                seller_fee_basis_points: 0,
                creators: None,
                collection: None,
                uses: None,
            }),
            None,
            Some(true),
        );

        let (_, stake_withdraw_bump_seed) =
            crate::find_withdraw_authority_program_address(program_id, stake_pool_info.key);

        let token_mint_authority_signer_seeds: &[&[_]] = &[
            &stake_pool_info.key.to_bytes()[..32],
            AUTHORITY_WITHDRAW,
            &[stake_withdraw_bump_seed],
        ];

        invoke_signed(
            &update_metadata_accounts_instruction,
            &[
                metadata_info.clone(),
                withdraw_authority_info.clone(),
                mpl_token_metadata_program_info.clone(),
            ],
            &[token_mint_authority_signer_seeds],
        )?;

        Ok(())
    }

    /// Processes [SetManager](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_set_manager(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let manager_info = next_account_info(account_info_iter)?;
        let new_manager_info = next_account_info(account_info_iter)?;
        let new_manager_fee_info = next_account_info(account_info_iter)?;

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        check_account_owner(new_manager_fee_info, &stake_pool.token_program_id)?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        stake_pool.check_manager(manager_info)?;
        if !new_manager_info.is_signer {
            msg!("New manager signature missing");
            return Err(StakePoolError::SignatureMissing.into());
        }

        stake_pool.check_manager_fee_info(new_manager_fee_info)?;

        stake_pool.manager = *new_manager_info.key;
        stake_pool.manager_fee_account = *new_manager_fee_info.key;
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;
        Ok(())
    }

    /// Processes [SetFee](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_set_fee(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        fee: FeeType,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let manager_info = next_account_info(account_info_iter)?;
        let clock = Clock::get()?;

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }
        stake_pool.check_manager(manager_info)?;

        if fee.can_only_change_next_epoch() && stake_pool.last_update_epoch < clock.epoch {
            return Err(StakePoolError::StakeListAndPoolOutOfDate.into());
        }

        fee.check_too_high()?;
        stake_pool.update_fee(&fee)?;
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;
        Ok(())
    }

    /// Processes [SetStaker](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_set_staker(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let set_staker_authority_info = next_account_info(account_info_iter)?;
        let new_staker_info = next_account_info(account_info_iter)?;

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }

        let staker_signed = stake_pool.check_staker(set_staker_authority_info);
        let manager_signed = stake_pool.check_manager(set_staker_authority_info);
        if staker_signed.is_err() && manager_signed.is_err() {
            return Err(StakePoolError::SignatureMissing.into());
        }
        stake_pool.staker = *new_staker_info.key;
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;
        Ok(())
    }

    /// Processes [SetFundingAuthority](enum.Instruction.html).
    #[inline(never)] // needed to avoid stack size violation
    fn process_set_funding_authority(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        funding_type: FundingType,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let stake_pool_info = next_account_info(account_info_iter)?;
        let manager_info = next_account_info(account_info_iter)?;

        let new_authority = next_account_info(account_info_iter)
            .ok()
            .map(|new_authority_account_info| *new_authority_account_info.key);

        check_account_owner(stake_pool_info, program_id)?;
        let mut stake_pool = try_from_slice_unchecked::<StakePool>(&stake_pool_info.data.borrow())?;
        if !stake_pool.is_valid() {
            return Err(StakePoolError::InvalidState.into());
        }
        stake_pool.check_manager(manager_info)?;
        match funding_type {
            FundingType::StakeDeposit => {
                stake_pool.stake_deposit_authority = new_authority.unwrap_or(
                    find_deposit_authority_program_address(program_id, stake_pool_info.key).0,
                );
            }
            FundingType::SolDeposit => stake_pool.sol_deposit_authority = new_authority,
            FundingType::SolWithdraw => stake_pool.sol_withdraw_authority = new_authority,
        }
        stake_pool.serialize(&mut *stake_pool_info.data.borrow_mut())?;
        Ok(())
    }

    /// Processes [Instruction](enum.Instruction.html).
    pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], input: &[u8]) -> ProgramResult {
        let instruction = StakePoolInstruction::try_from_slice(input)?;
        match instruction {
            StakePoolInstruction::Initialize {
                fee,
                withdrawal_fee,
                deposit_fee,
                referral_fee,
                max_validators,
            } => {
                msg!("Instruction: Initialize stake pool");
                Self::process_initialize(
                    program_id,
                    accounts,
                    fee,
                    withdrawal_fee,
                    deposit_fee,
                    referral_fee,
                    max_validators,
                )
            }
            StakePoolInstruction::AddValidatorToPool(seed) => {
                msg!("Instruction: AddValidatorToPool");
                Self::process_add_validator_to_pool(program_id, accounts, seed)
            }
            StakePoolInstruction::RemoveValidatorFromPool => {
                msg!("Instruction: RemoveValidatorFromPool");
                Self::process_remove_validator_from_pool(program_id, accounts)
            }
            StakePoolInstruction::DecreaseValidatorStake {
                lamports,
                transient_stake_seed,
            } => {
                msg!("Instruction: DecreaseValidatorStake");
                Self::process_decrease_validator_stake(
                    program_id,
                    accounts,
                    lamports,
                    transient_stake_seed,
                )
            }
            StakePoolInstruction::IncreaseValidatorStake {
                lamports,
                transient_stake_seed,
            } => {
                msg!("Instruction: IncreaseValidatorStake");
                Self::process_increase_validator_stake(
                    program_id,
                    accounts,
                    lamports,
                    transient_stake_seed,
                )
            }
            StakePoolInstruction::SetPreferredValidator {
                validator_type,
                validator_vote_address,
            } => {
                msg!("Instruction: SetPreferredValidator");
                Self::process_set_preferred_validator(
                    program_id,
                    accounts,
                    validator_type,
                    validator_vote_address,
                )
            }
            StakePoolInstruction::UpdateValidatorListBalance {
                start_index,
                no_merge,
            } => {
                msg!("Instruction: UpdateValidatorListBalance");
                Self::process_update_validator_list_balance(
                    program_id,
                    accounts,
                    start_index,
                    no_merge,
                )
            }
            StakePoolInstruction::UpdateStakePoolBalance => {
                msg!("Instruction: UpdateStakePoolBalance");
                Self::process_update_stake_pool_balance(program_id, accounts)
            }
            StakePoolInstruction::CleanupRemovedValidatorEntries => {
                msg!("Instruction: CleanupRemovedValidatorEntries");
                Self::process_cleanup_removed_validator_entries(program_id, accounts)
            }
            StakePoolInstruction::DepositStake => {
                msg!("Instruction: DepositStake");
                Self::process_deposit_stake(program_id, accounts)
            }
            StakePoolInstruction::WithdrawStake(amount) => {
                msg!("Instruction: WithdrawStake");
                Self::process_withdraw_stake(program_id, accounts, amount)
            }
            StakePoolInstruction::SetFee { fee } => {
                msg!("Instruction: SetFee");
                Self::process_set_fee(program_id, accounts, fee)
            }
            StakePoolInstruction::SetManager => {
                msg!("Instruction: SetManager");
                Self::process_set_manager(program_id, accounts)
            }
            StakePoolInstruction::SetStaker => {
                msg!("Instruction: SetStaker");
                Self::process_set_staker(program_id, accounts)
            }
            StakePoolInstruction::SetFundingAuthority(funding_type) => {
                msg!("Instruction: SetFundingAuthority");
                Self::process_set_funding_authority(program_id, accounts, funding_type)
            }
            StakePoolInstruction::DepositSol(lamports) => {
                msg!("Instruction: DepositSol");
                Self::process_deposit_sol(program_id, accounts, lamports)
            }
            StakePoolInstruction::WithdrawSol(pool_tokens) => {
                msg!("Instruction: WithdrawSol");
                Self::process_withdraw_sol(program_id, accounts, pool_tokens)
            }
            StakePoolInstruction::CreateTokenMetadata { name, symbol, uri } => {
                msg!("Instruction: CreateTokenMetadata");
                Self::process_create_pool_token_metadata(program_id, accounts, name, symbol, uri)
            }
            StakePoolInstruction::UpdateTokenMetadata { name, symbol, uri } => {
                msg!("Instruction: UpdateTokenMetadata");
                Self::process_update_pool_token_metadata(program_id, accounts, name, symbol, uri)
            }
        }
    }
}

impl PrintProgramError for StakePoolError {
    fn print<E>(&self)
    where
        E: 'static + std::error::Error + DecodeError<E> + PrintProgramError + FromPrimitive,
    {
        match self {
            StakePoolError::AlreadyInUse => msg!("Error: The account cannot be initialized because it is already being used"),
            StakePoolError::InvalidProgramAddress => msg!("Error: The program address provided doesn't match the value generated by the program"),
            StakePoolError::InvalidState => msg!("Error: The stake pool state is invalid"),
            StakePoolError::CalculationFailure => msg!("Error: The calculation failed"),
            StakePoolError::FeeTooHigh => msg!("Error: Stake pool fee > 1"),
            StakePoolError::WrongAccountMint => msg!("Error: Token account is associated with the wrong mint"),
            StakePoolError::WrongManager => msg!("Error: Wrong pool manager account"),
            StakePoolError::SignatureMissing => msg!("Error: Required signature is missing"),
            StakePoolError::InvalidValidatorStakeList => msg!("Error: Invalid validator stake list account"),
            StakePoolError::InvalidFeeAccount => msg!("Error: Invalid manager fee account"),
            StakePoolError::WrongPoolMint => msg!("Error: Specified pool mint account is wrong"),
            StakePoolError::WrongStakeState => msg!("Error: Stake account is not in the state expected by the program"),
            StakePoolError::UserStakeNotActive => msg!("Error: User stake is not active"),
            StakePoolError::ValidatorAlreadyAdded => msg!("Error: Stake account voting for this validator already exists in the pool"),
            StakePoolError::ValidatorNotFound => msg!("Error: Stake account for this validator not found in the pool"),
            StakePoolError::InvalidStakeAccountAddress => msg!("Error: Stake account address not properly derived from the validator address"),
            StakePoolError::StakeListOutOfDate => msg!("Error: Identify validator stake accounts with old balances and update them"),
            StakePoolError::StakeListAndPoolOutOfDate => msg!("Error: First update old validator stake account balances and then pool stake balance"),
            StakePoolError::UnknownValidatorStakeAccount => {
                msg!("Error: Validator stake account is not found in the list storage")
            }
            StakePoolError::WrongMintingAuthority => msg!("Error: Wrong minting authority set for mint pool account"),
            StakePoolError::UnexpectedValidatorListAccountSize=> msg!("Error: The size of the given validator stake list does match the expected amount"),
            StakePoolError::WrongStaker=> msg!("Error: Wrong pool staker account"),
            StakePoolError::NonZeroPoolTokenSupply => msg!("Error: Pool token supply is not zero on initialization"),
            StakePoolError::StakeLamportsNotEqualToMinimum => msg!("Error: The lamports in the validator stake account is not equal to the minimum"),
            StakePoolError::IncorrectDepositVoteAddress => msg!("Error: The provided deposit stake account is not delegated to the preferred deposit vote account"),
            StakePoolError::IncorrectWithdrawVoteAddress => msg!("Error: The provided withdraw stake account is not the preferred deposit vote account"),
            StakePoolError::InvalidMintFreezeAuthority => msg!("Error: The mint has an invalid freeze authority"),
            StakePoolError::FeeIncreaseTooHigh => msg!("Error: The fee cannot increase by a factor exceeding the stipulated ratio"),
            StakePoolError::WithdrawalTooSmall => msg!("Error: Not enough pool tokens provided to withdraw 1-lamport stake"),
            StakePoolError::DepositTooSmall => msg!("Error: Not enough lamports provided for deposit to result in one pool token"),
            StakePoolError::InvalidStakeDepositAuthority => msg!("Error: Provided stake deposit authority does not match the program's"),
            StakePoolError::InvalidSolDepositAuthority => msg!("Error: Provided sol deposit authority does not match the program's"),
            StakePoolError::InvalidPreferredValidator => msg!("Error: Provided preferred validator is invalid"),
            StakePoolError::TransientAccountInUse => msg!("Error: Provided validator stake account already has a transient stake account in use"),
            StakePoolError::InvalidSolWithdrawAuthority => msg!("Error: Provided sol withdraw authority does not match the program's"),
            StakePoolError::SolWithdrawalTooLarge => msg!("Error: Too much SOL withdrawn from the stake pool's reserve account"),
            StakePoolError::InvalidMetadataAccount => msg!("Error: Metadata account derived from pool mint account does not match the one passed to program"),
            StakePoolError::UnsupportedMintExtension => msg!("Error: mint has an unsupported extension"),
            StakePoolError::UnsupportedFeeAccountExtension => msg!("Error: fee account has an unsupported extension"),
        }
    }
}
