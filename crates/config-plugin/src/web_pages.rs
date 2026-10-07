use crate::store::FileConfigService;
use eve_web_panel_api::*;
use std::sync::Arc;

pub(crate) struct ConfigPages {
    pub service: Arc<FileConfigService>,
    pub permit: PageWritePermit,
}
impl PluginPages for ConfigPages {
    fn pages(&self) -> PanelResult<Vec<PageDescriptor>> {
        self.service.web_pages()
    }
    fn read(&self, page: &str) -> PanelResult<PluginPage> {
        self.service.web_read(page)
    }
    fn save(&self, request: &PageSaveRequest, permit: &PageWritePermit) -> PanelResult<PageSaved> {
        if !self.permit.same_grant(permit) {
            return Err(PanelError::Forbidden);
        }
        self.service.web_save(request)
    }
}
