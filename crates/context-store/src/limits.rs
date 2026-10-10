use crate::{SiteContext, SiteEnvelope};
use serde::{Deserialize, Serialize};
use std::{borrow::Borrow, collections::BTreeMap, io, mem::size_of};

/// Per-profile limits. Byte accounting includes owned capacities and a
/// conservative allocation allowance for every B-tree entry; it is not RSS.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextLimits {
    pub max_file_bytes: usize,
    pub max_site_records: usize,
    pub max_resident_sites: usize,
    pub max_resident_bytes: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 2 * 1024 * 1024,
            max_site_records: 16_384,
            max_resident_sites: 256,
            max_resident_bytes: 64 * 1024 * 1024,
        }
    }
}

impl ContextLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_file_bytes == 0
            || self.max_site_records == 0
            || self.max_resident_sites == 0
            || self.max_resident_bytes == 0
        {
            return Err("context limits must be positive");
        }
        Ok(())
    }

    pub(crate) fn check_site(&self, site: &SiteContext) -> Result<(), &'static str> {
        let mut records = site.pages.len().saturating_add(site.challenges.len());
        for page in site.pages.values() {
            records = records.saturating_add(page.forms.len());
            for form in page.forms.values() {
                records = records.saturating_add(form.controls.len());
                for control in &form.controls {
                    records = records.saturating_add(control.intents.len());
                }
            }
        }
        if records > self.max_site_records {
            return Err("context site exceeds structural record limit");
        }
        Ok(())
    }

    pub(crate) fn check_envelope<Site: Serialize + Borrow<SiteContext>>(
        &self,
        envelope: &SiteEnvelope<Site>,
    ) -> Result<(), String> {
        self.check_site(envelope.site.borrow())
            .map_err(str::to_string)?;
        serde_json::to_writer(BudgetWriter(self.max_file_bytes), envelope)
            .map_err(|_| "context site exceeds file byte limit".to_string())
    }
}

struct BudgetWriter(usize);
impl io::Write for BudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("context file byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Charge a whole maximum-sized node per entry, including internal edges.
// This overcounts sparsely populated maps instead of undercounting them.
pub(crate) fn map_bytes<K, V>(map: &BTreeMap<K, V>) -> usize {
    map.len().saturating_mul(node_bytes::<K, V>())
}
pub(crate) fn node_bytes<K, V>() -> usize {
    (size_of::<K>() + size_of::<V>())
        .saturating_mul(11)
        .saturating_add(12 * size_of::<usize>() + 64)
}

pub(crate) fn site_bytes(site: &SiteContext) -> usize {
    let mut bytes = map_bytes(&site.pages).saturating_add(map_bytes(&site.challenges));
    for (key, page) in &site.pages {
        bytes = bytes
            .saturating_add(key.capacity())
            .saturating_add(map_bytes(&page.forms));
        for (key, form) in &page.forms {
            bytes = bytes.saturating_add(key.capacity()).saturating_add(
                form.controls
                    .capacity()
                    .saturating_mul(size_of::<crate::ControlContext>()),
            );
            for control in &form.controls {
                bytes = bytes
                    .saturating_add(control.role.capacity())
                    .saturating_add(control.accessible_name.capacity())
                    .saturating_add(control.form_membership.capacity())
                    .saturating_add(map_bytes(&control.intents));
                for key in control.intents.keys() {
                    bytes = bytes.saturating_add(key.capacity());
                }
            }
        }
    }
    for key in site.challenges.keys() {
        bytes = bytes.saturating_add(key.capacity());
    }
    bytes
}
