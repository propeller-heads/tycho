use crate::lunarbase::attributes::AttributeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    pub updated_attributes: AttributeMap,
}
