//! Branch-specific commands. Navigation stays in the parent module because all
//! three verbs share it; branch removal lives here because its local and upstream
//! targets have a safety contract of their own.

use super::picker::{MultiItem, interactive_keys, multi_select};
use super::{
    Confirmation, DefaultAnswer, confirm, interactive_term, removal, select_removal_locals,
};
use crate::grammar::BranchRemoval;
use crate::{AppResult, Error, git};

pub(crate) fn run_rm(options: &BranchRemoval) -> AppResult<()> {
    let worktrees = git::worktree_list()?;
    let current = git::current_branch()?;
    let local = git::local_branches()?;
    if local.is_empty() {
        eprintln!("No local branches to remove.");
        return Ok(());
    }
    let upstream = if options.upstream() {
        removal::UpstreamInterest::Requested
    } else if interactive_term().is_some() {
        removal::UpstreamInterest::Offer
    } else {
        removal::UpstreamInterest::None
    };
    let assessment = removal::assess(removal::Request::Branches(removal::BranchRequest::new(
        local,
        worktrees,
        current.as_deref(),
        upstream,
    )))?;
    let Some(selection) = select_removal_locals(
        &assessment,
        options.target(),
        git::Forcing::from(options.force()),
        "Remove local branches (space to toggle, →/← all/none)",
    )?
    else {
        return Ok(());
    };
    let choice = selection.into_choice();
    let pending = assessment.choose(choice)?;
    for notice in pending.notices() {
        eprintln!("{notice}");
    }
    let Some(upstream) = select_upstream(&pending, options)? else {
        return Ok(());
    };
    finish(pending, upstream)
}

fn select_upstream(
    pending: &removal::Pending,
    options: &BranchRemoval,
) -> AppResult<Option<removal::UpstreamChoice>> {
    if pending.upstream_offers().is_empty() {
        return Ok(Some(removal::UpstreamChoice::keep()));
    }
    if options.force() && options.upstream() {
        return Ok(Some(removal::UpstreamChoice::selected(
            pending
                .upstream_offers()
                .iter()
                .map(removal::UpstreamOffer::id)
                .collect(),
        )));
    }
    if options.target().is_some() {
        let offer = &pending.upstream_offers()[0];
        eprintln!("{}", offer.warning());
        if interactive_term().is_none() {
            return Err(Error::Unconfirmed(
                "upstream deletion was requested without a terminal; pass --force to confirm it"
                    .into(),
            ));
        }
        // Asking for `--upstream` already said yes; Enter keeps that answer.
        let default = if options.upstream() {
            DefaultAnswer::Yes
        } else {
            DefaultAnswer::No
        };
        return Ok(match confirm(offer.question(), default)? {
            Confirmation::Accepted => Some(removal::UpstreamChoice::selected(vec![offer.id()])),
            Confirmation::Cancelled => None,
            Confirmation::Declined => Some(removal::UpstreamChoice::keep()),
        });
    }

    let Some(keys) = interactive_keys() else {
        if options.upstream() {
            return Err(Error::Unconfirmed(
                "upstream deletion was requested without a terminal; pass --force to confirm it"
                    .into(),
            ));
        }
        return Ok(Some(removal::UpstreamChoice::keep()));
    };
    let items: Vec<MultiItem> = pending
        .upstream_offers()
        .iter()
        .map(|offer| MultiItem {
            label: offer.label().to_string(),
            selected: options.upstream(),
            disabled: false,
        })
        .collect();
    let Some(selected) = multi_select(
        "Also delete upstream branches? (space to toggle, →/← all/none)",
        Some("Each selected row removes a shared upstream ref"),
        &items,
        keys,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(removal::UpstreamChoice::selected(
        selected
            .into_iter()
            .map(|index| pending.upstream_offers()[index].id())
            .collect(),
    )))
}

fn finish(pending: removal::Pending, upstream: removal::UpstreamChoice) -> AppResult<()> {
    let outcome = pending
        .finish(upstream, removal::StderrReporter)
        .map_err(removal::FinishFailure::into_error)?;
    if outcome.failed() {
        Err(Error::RemovalFailed)
    } else {
        Ok(())
    }
}
