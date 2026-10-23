//! [`Money`] as a column: an exact `numeric(12, 2)` in baht, never floating point (DR-01).

use sea_orm::prelude::Decimal;
use sea_orm::sea_query::{ArrayType, ColumnType, Nullable, ValueType, ValueTypeErr};
use sea_orm::{
    ActiveValue, ColIdx, DbErr, IntoActiveValue, QueryResult, TryGetError, TryGetable, Value,
};

use crate::thai::Money;

/// Up to ฿9,999,999,999.99, far above the largest possible order (FR-PROD-03, FR-CART-07).
const PRECISION: u32 = 12;
/// Satang.
const SCALE: u32 = 2;

impl From<Money> for Value {
    fn from(money: Money) -> Self {
        to_decimal(money).into()
    }
}

impl TryGetable for Money {
    fn try_get_by<I: ColIdx>(res: &QueryResult, index: I) -> Result<Self, TryGetError> {
        let decimal = Decimal::try_get_by(res, index)?;
        from_decimal(decimal).ok_or_else(|| {
            TryGetError::DbErr(DbErr::Type(format!("{decimal} is not an amount of satang")))
        })
    }
}

impl ValueType for Money {
    fn try_from(value: Value) -> Result<Self, ValueTypeErr> {
        from_decimal(<Decimal as ValueType>::try_from(value)?).ok_or(ValueTypeErr)
    }

    fn type_name() -> String {
        "Money".to_owned()
    }

    fn array_type() -> ArrayType {
        ArrayType::Decimal
    }

    fn column_type() -> ColumnType {
        ColumnType::Decimal(Some((PRECISION, SCALE)))
    }
}

impl Nullable for Money {
    fn null() -> Value {
        <Decimal as Nullable>::null()
    }
}

impl IntoActiveValue<Money> for Money {
    fn into_active_value(self) -> ActiveValue<Money> {
        ActiveValue::Set(self)
    }
}

pub(super) fn to_decimal(money: Money) -> Decimal {
    Decimal::new(money.satang(), SCALE)
}

/// `None` for fractions of a satang or an amount that doesn't fit.
pub(super) fn from_decimal(decimal: Decimal) -> Option<Money> {
    let mut satang = decimal;
    satang.rescale(SCALE);
    if satang != decimal {
        return None;
    }
    satang.mantissa().try_into().ok().map(Money::from_satang)
}
