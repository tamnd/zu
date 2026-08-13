//! Generated from `artifacts/gql-conditions.xml`, the ISO/IEC
//! 39075:2024 conditions artifact. Do not edit by hand: run
//! `ZU_UPDATE_GQLSTATUS=1 cargo test -p zu-common --test gqlstatus_table`.
//!
//! Codes and natural-language names are the standard's, verbatim.

use super::{Condition, GqlStatus, Severity};

/// Every GQLSTATUS value the standard defines, in code order.
pub(super) static CONDITIONS: &[Condition] = &[
    Condition {
        code: "00001",
        severity: Severity::Success,
        class: "successful completion",
        subclass: Some("omitted result"),
    },
    Condition {
        code: "01004",
        severity: Severity::Warning,
        class: "warning",
        subclass: Some("string data, right truncation"),
    },
    Condition {
        code: "01G03",
        severity: Severity::Warning,
        class: "warning",
        subclass: Some("graph does not exist"),
    },
    Condition {
        code: "01G04",
        severity: Severity::Warning,
        class: "warning",
        subclass: Some("graph type does not exist"),
    },
    Condition {
        code: "01G11",
        severity: Severity::Warning,
        class: "warning",
        subclass: Some("null value eliminated in set function"),
    },
    Condition {
        code: "02000",
        severity: Severity::NoData,
        class: "no data",
        subclass: None,
    },
    Condition {
        code: "03000",
        severity: Severity::Informational,
        class: "informational",
        subclass: None,
    },
    Condition {
        code: "08007",
        severity: Severity::Exception,
        class: "connection exception",
        subclass: Some("transaction resolution unknown"),
    },
    Condition {
        code: "22001",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("string data, right truncation"),
    },
    Condition {
        code: "22003",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("numeric value out of range"),
    },
    Condition {
        code: "22004",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("null value not allowed"),
    },
    Condition {
        code: "22007",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid date, time, or, datetime format"),
    },
    Condition {
        code: "22008",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("datetime field overflow"),
    },
    Condition {
        code: "22011",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("substring error"),
    },
    Condition {
        code: "22012",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("division by zero"),
    },
    Condition {
        code: "22015",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("interval field overflow"),
    },
    Condition {
        code: "22018",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid character value for cast"),
    },
    Condition {
        code: "2201E",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid argument for natural logarithm"),
    },
    Condition {
        code: "2201F",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid argument for power function"),
    },
    Condition {
        code: "22027",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("trim error"),
    },
    Condition {
        code: "2202F",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("array data, right truncation"),
    },
    Condition {
        code: "22G02",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("negative limit value"),
    },
    Condition {
        code: "22G03",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid value type"),
    },
    Condition {
        code: "22G04",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("values not comparable"),
    },
    Condition {
        code: "22G05",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid date, time, or datetime function field name"),
    },
    Condition {
        code: "22G06",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid datetime function value"),
    },
    Condition {
        code: "22G07",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid duration function field name"),
    },
    Condition {
        code: "22G0B",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("list data, right truncation"),
    },
    Condition {
        code: "22G0C",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("list element error"),
    },
    Condition {
        code: "22G0F",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid number of paths or groups"),
    },
    Condition {
        code: "22G0H",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid duration format"),
    },
    Condition {
        code: "22G0M",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("multiple assignments to a graph element property"),
    },
    Condition {
        code: "22G0N",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("number of node labels below supported minimum"),
    },
    Condition {
        code: "22G0P",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("number of node labels exceeds supported maximum"),
    },
    Condition {
        code: "22G0Q",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("number of edge labels below supported minimum"),
    },
    Condition {
        code: "22G0R",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("number of edge labels exceeds supported maximum"),
    },
    Condition {
        code: "22G0S",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("number of node properties exceeds supported maximum"),
    },
    Condition {
        code: "22G0T",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("number of edge properties exceeds supported maximum"),
    },
    Condition {
        code: "22G0U",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("record fields do not match"),
    },
    Condition {
        code: "22G0V",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("reference value, invalid base type"),
    },
    Condition {
        code: "22G0W",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("reference value, invalid constrained type"),
    },
    Condition {
        code: "22G0X",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("record data, field unassignable"),
    },
    Condition {
        code: "22G0Y",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("record data, field missing"),
    },
    Condition {
        code: "22G0Z",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("malformed path"),
    },
    Condition {
        code: "22G10",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("path data, right truncation"),
    },
    Condition {
        code: "22G11",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("reference value, referent deleted"),
    },
    Condition {
        code: "22G12",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid value type"),
    },
    Condition {
        code: "22G13",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("invalid group variable value"),
    },
    Condition {
        code: "22G14",
        severity: Severity::Exception,
        class: "data exception",
        subclass: Some("incompatible temporal instant unit groups"),
    },
    Condition {
        code: "25G01",
        severity: Severity::Exception,
        class: "invalid transaction state",
        subclass: Some("active GQL-transaction"),
    },
    Condition {
        code: "25G02",
        severity: Severity::Exception,
        class: "invalid transaction state",
        subclass: Some("catalog and data statement mixing not supported"),
    },
    Condition {
        code: "25G03",
        severity: Severity::Exception,
        class: "invalid transaction state",
        subclass: Some("read-only GQL-transaction"),
    },
    Condition {
        code: "25G04",
        severity: Severity::Exception,
        class: "invalid transaction state",
        subclass: Some("accessing multiple graphs not supported"),
    },
    Condition {
        code: "2D000",
        severity: Severity::Exception,
        class: "invalid transaction termination",
        subclass: None,
    },
    Condition {
        code: "40003",
        severity: Severity::Exception,
        class: "transaction rollback",
        subclass: Some("statement completion unknown"),
    },
    Condition {
        code: "42001",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("invalid syntax"),
    },
    Condition {
        code: "42002",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("invalid reference"),
    },
    Condition {
        code: "42004",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("use of visually confusable identifiers"),
    },
    Condition {
        code: "42006",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of edge labels below supported minimum"),
    },
    Condition {
        code: "42007",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of edge labels exceeds supported maximum"),
    },
    Condition {
        code: "42008",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of edge properties exceeds supported maximum"),
    },
    Condition {
        code: "42009",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of node labels below supported minimum"),
    },
    Condition {
        code: "42010",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of node labels exceeds supported maximum"),
    },
    Condition {
        code: "42011",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of node properties exceeds supported maximum"),
    },
    Condition {
        code: "42012",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of node type key labels below supported minimum"),
    },
    Condition {
        code: "42013",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of node type key labels exceeds supported maximum"),
    },
    Condition {
        code: "42014",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of edge type key labels below supported minimum"),
    },
    Condition {
        code: "42015",
        severity: Severity::Exception,
        class: "syntax error or access rule violation",
        subclass: Some("number of edge type key labels exceeds supported maximum"),
    },
    Condition {
        code: "G1001",
        severity: Severity::Exception,
        class: "dependent object error",
        subclass: Some("edges still exist"),
    },
    Condition {
        code: "G1002",
        severity: Severity::Exception,
        class: "dependent object error",
        subclass: Some("endpoint node is deleted"),
    },
    Condition {
        code: "G1003",
        severity: Severity::Exception,
        class: "dependent object error",
        subclass: Some("endpoint node not in current working graph"),
    },
    Condition {
        code: "G2000",
        severity: Severity::Exception,
        class: "graph type violation",
        subclass: None,
    },
];

/// One constant per condition, named `C` followed by the code.
/// The doc comment on each is the standard's own wording.
pub mod codes {
    use super::GqlStatus;

    /// `00001` successful completion, omitted result
    pub const C00001: GqlStatus = GqlStatus(0);
    /// `01004` warning, string data, right truncation
    pub const C01004: GqlStatus = GqlStatus(1);
    /// `01G03` warning, graph does not exist
    pub const C01G03: GqlStatus = GqlStatus(2);
    /// `01G04` warning, graph type does not exist
    pub const C01G04: GqlStatus = GqlStatus(3);
    /// `01G11` warning, null value eliminated in set function
    pub const C01G11: GqlStatus = GqlStatus(4);
    /// `02000` no data
    pub const C02000: GqlStatus = GqlStatus(5);
    /// `03000` informational
    pub const C03000: GqlStatus = GqlStatus(6);
    /// `08007` connection exception, transaction resolution unknown
    pub const C08007: GqlStatus = GqlStatus(7);
    /// `22001` data exception, string data, right truncation
    pub const C22001: GqlStatus = GqlStatus(8);
    /// `22003` data exception, numeric value out of range
    pub const C22003: GqlStatus = GqlStatus(9);
    /// `22004` data exception, null value not allowed
    pub const C22004: GqlStatus = GqlStatus(10);
    /// `22007` data exception, invalid date, time, or, datetime format
    pub const C22007: GqlStatus = GqlStatus(11);
    /// `22008` data exception, datetime field overflow
    pub const C22008: GqlStatus = GqlStatus(12);
    /// `22011` data exception, substring error
    pub const C22011: GqlStatus = GqlStatus(13);
    /// `22012` data exception, division by zero
    pub const C22012: GqlStatus = GqlStatus(14);
    /// `22015` data exception, interval field overflow
    pub const C22015: GqlStatus = GqlStatus(15);
    /// `22018` data exception, invalid character value for cast
    pub const C22018: GqlStatus = GqlStatus(16);
    /// `2201E` data exception, invalid argument for natural logarithm
    pub const C2201E: GqlStatus = GqlStatus(17);
    /// `2201F` data exception, invalid argument for power function
    pub const C2201F: GqlStatus = GqlStatus(18);
    /// `22027` data exception, trim error
    pub const C22027: GqlStatus = GqlStatus(19);
    /// `2202F` data exception, array data, right truncation
    pub const C2202F: GqlStatus = GqlStatus(20);
    /// `22G02` data exception, negative limit value
    pub const C22G02: GqlStatus = GqlStatus(21);
    /// `22G03` data exception, invalid value type
    pub const C22G03: GqlStatus = GqlStatus(22);
    /// `22G04` data exception, values not comparable
    pub const C22G04: GqlStatus = GqlStatus(23);
    /// `22G05` data exception, invalid date, time, or datetime function field name
    pub const C22G05: GqlStatus = GqlStatus(24);
    /// `22G06` data exception, invalid datetime function value
    pub const C22G06: GqlStatus = GqlStatus(25);
    /// `22G07` data exception, invalid duration function field name
    pub const C22G07: GqlStatus = GqlStatus(26);
    /// `22G0B` data exception, list data, right truncation
    pub const C22G0B: GqlStatus = GqlStatus(27);
    /// `22G0C` data exception, list element error
    pub const C22G0C: GqlStatus = GqlStatus(28);
    /// `22G0F` data exception, invalid number of paths or groups
    pub const C22G0F: GqlStatus = GqlStatus(29);
    /// `22G0H` data exception, invalid duration format
    pub const C22G0H: GqlStatus = GqlStatus(30);
    /// `22G0M` data exception, multiple assignments to a graph element property
    pub const C22G0M: GqlStatus = GqlStatus(31);
    /// `22G0N` data exception, number of node labels below supported minimum
    pub const C22G0N: GqlStatus = GqlStatus(32);
    /// `22G0P` data exception, number of node labels exceeds supported maximum
    pub const C22G0P: GqlStatus = GqlStatus(33);
    /// `22G0Q` data exception, number of edge labels below supported minimum
    pub const C22G0Q: GqlStatus = GqlStatus(34);
    /// `22G0R` data exception, number of edge labels exceeds supported maximum
    pub const C22G0R: GqlStatus = GqlStatus(35);
    /// `22G0S` data exception, number of node properties exceeds supported maximum
    pub const C22G0S: GqlStatus = GqlStatus(36);
    /// `22G0T` data exception, number of edge properties exceeds supported maximum
    pub const C22G0T: GqlStatus = GqlStatus(37);
    /// `22G0U` data exception, record fields do not match
    pub const C22G0U: GqlStatus = GqlStatus(38);
    /// `22G0V` data exception, reference value, invalid base type
    pub const C22G0V: GqlStatus = GqlStatus(39);
    /// `22G0W` data exception, reference value, invalid constrained type
    pub const C22G0W: GqlStatus = GqlStatus(40);
    /// `22G0X` data exception, record data, field unassignable
    pub const C22G0X: GqlStatus = GqlStatus(41);
    /// `22G0Y` data exception, record data, field missing
    pub const C22G0Y: GqlStatus = GqlStatus(42);
    /// `22G0Z` data exception, malformed path
    pub const C22G0Z: GqlStatus = GqlStatus(43);
    /// `22G10` data exception, path data, right truncation
    pub const C22G10: GqlStatus = GqlStatus(44);
    /// `22G11` data exception, reference value, referent deleted
    pub const C22G11: GqlStatus = GqlStatus(45);
    /// `22G12` data exception, invalid value type
    pub const C22G12: GqlStatus = GqlStatus(46);
    /// `22G13` data exception, invalid group variable value
    pub const C22G13: GqlStatus = GqlStatus(47);
    /// `22G14` data exception, incompatible temporal instant unit groups
    pub const C22G14: GqlStatus = GqlStatus(48);
    /// `25G01` invalid transaction state, active GQL-transaction
    pub const C25G01: GqlStatus = GqlStatus(49);
    /// `25G02` invalid transaction state, catalog and data statement mixing not supported
    pub const C25G02: GqlStatus = GqlStatus(50);
    /// `25G03` invalid transaction state, read-only GQL-transaction
    pub const C25G03: GqlStatus = GqlStatus(51);
    /// `25G04` invalid transaction state, accessing multiple graphs not supported
    pub const C25G04: GqlStatus = GqlStatus(52);
    /// `2D000` invalid transaction termination
    pub const C2D000: GqlStatus = GqlStatus(53);
    /// `40003` transaction rollback, statement completion unknown
    pub const C40003: GqlStatus = GqlStatus(54);
    /// `42001` syntax error or access rule violation, invalid syntax
    pub const C42001: GqlStatus = GqlStatus(55);
    /// `42002` syntax error or access rule violation, invalid reference
    pub const C42002: GqlStatus = GqlStatus(56);
    /// `42004` syntax error or access rule violation, use of visually confusable identifiers
    pub const C42004: GqlStatus = GqlStatus(57);
    /// `42006` syntax error or access rule violation, number of edge labels below supported minimum
    pub const C42006: GqlStatus = GqlStatus(58);
    /// `42007` syntax error or access rule violation, number of edge labels exceeds supported maximum
    pub const C42007: GqlStatus = GqlStatus(59);
    /// `42008` syntax error or access rule violation, number of edge properties exceeds supported maximum
    pub const C42008: GqlStatus = GqlStatus(60);
    /// `42009` syntax error or access rule violation, number of node labels below supported minimum
    pub const C42009: GqlStatus = GqlStatus(61);
    /// `42010` syntax error or access rule violation, number of node labels exceeds supported maximum
    pub const C42010: GqlStatus = GqlStatus(62);
    /// `42011` syntax error or access rule violation, number of node properties exceeds supported maximum
    pub const C42011: GqlStatus = GqlStatus(63);
    /// `42012` syntax error or access rule violation, number of node type key labels below supported minimum
    pub const C42012: GqlStatus = GqlStatus(64);
    /// `42013` syntax error or access rule violation, number of node type key labels exceeds supported maximum
    pub const C42013: GqlStatus = GqlStatus(65);
    /// `42014` syntax error or access rule violation, number of edge type key labels below supported minimum
    pub const C42014: GqlStatus = GqlStatus(66);
    /// `42015` syntax error or access rule violation, number of edge type key labels exceeds supported maximum
    pub const C42015: GqlStatus = GqlStatus(67);
    /// `G1001` dependent object error, edges still exist
    pub const CG1001: GqlStatus = GqlStatus(68);
    /// `G1002` dependent object error, endpoint node is deleted
    pub const CG1002: GqlStatus = GqlStatus(69);
    /// `G1003` dependent object error, endpoint node not in current working graph
    pub const CG1003: GqlStatus = GqlStatus(70);
    /// `G2000` graph type violation
    pub const CG2000: GqlStatus = GqlStatus(71);
}
