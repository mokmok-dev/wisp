import Foundation
import SQLite3
import WispCore

/// Storage-layer error type.
public enum StorageError: Error, CustomStringConvertible, Equatable {
  /// A `sqlite3_*` call failed. `message` is `sqlite3_errmsg`.
  case sqlite(code: Int32, message: String)
  /// A filesystem operation on the storage root failed.
  case io(message: String)
  /// A migration step failed; the database stays at its previous version.
  case migration(target: UInt32, message: String)
  /// The `source` column held a value outside `{"mic","system"}`. Only
  /// possible via direct database edits.
  case invalidSourceLabel(String)
  /// A segment passed to `finaliseSession` belongs to another session.
  case sessionMismatch(expected: SessionID, actual: SessionID)
  /// The session to finalise does not exist.
  case sessionNotFound(SessionID)
  /// A stored timestamp column held text that is not a date.
  case invalidTimestamp(String)

  public var description: String {
    switch self {
    case .sqlite(let code, let message):
      "sqlite error \(code): \(message)"
    case .io(let message):
      "io error: \(message)"
    case .migration(let target, let message):
      "migration failed at target version \(target): \(message)"
    case .invalidSourceLabel(let label):
      "unrecognized source label: \(label)"
    case .sessionMismatch(let expected, let actual):
      "segment belongs to session \(actual), expected session \(expected)"
    case .sessionNotFound(let id):
      "session \(id) does not exist"
    case .invalidTimestamp(let text):
      "unrecognized timestamp: \(text)"
    }
  }
}

/// One bound value or one column read.
enum SQLiteValue {
  case integer(Int64)
  case real(Double)
  case text(String)
  case null
}

/// A prepared statement's current row.
struct Row {
  let statement: OpaquePointer

  func integer(at index: Int32) -> Int64 {
    sqlite3_column_int64(statement, index)
  }

  func double(at index: Int32) -> Double {
    sqlite3_column_double(statement, index)
  }

  func text(at index: Int32) -> String {
    guard let pointer = sqlite3_column_text(statement, index) else { return "" }
    return String(cString: pointer)
  }

  func optionalText(at index: Int32) -> String? {
    guard sqlite3_column_type(statement, index) != SQLITE_NULL,
      let pointer = sqlite3_column_text(statement, index)
    else {
      return nil
    }
    return String(cString: pointer)
  }
}

/// Owns one `sqlite3*` handle. `Storage` is its only owner; the repositories
/// borrow it.
final class Connection {
  private(set) var handle: OpaquePointer?

  init(path: String) throws {
    var handle: OpaquePointer?
    let code = sqlite3_open(path, &handle)
    guard code == SQLITE_OK, let handle else {
      let message =
        handle.map { String(cString: sqlite3_errmsg($0)) }
        ?? String(cString: sqlite3_errstr(code))
      sqlite3_close_v2(handle)
      throw StorageError.sqlite(code: code, message: message)
    }
    self.handle = handle
    // rusqlite installs a 5000 ms busy handler by default. Without one the
    // first `SQLITE_BUSY` — a second instance, Time Machine, a shell `sqlite3`
    // — aborts the write and parks the UI in a failed state.
    sqlite3_busy_timeout(handle, 5000)
  }

  deinit {
    sqlite3_close_v2(handle)
  }

  /// The last error message reported by SQLite.
  var errorMessage: String {
    guard let handle else { return "connection is closed" }
    return String(cString: sqlite3_errmsg(handle))
  }

  func lastError(_ code: Int32) -> StorageError {
    .sqlite(code: code, message: errorMessage)
  }

  /// Run one or more statements with no bindings. Used for pragmas and
  /// migration batches.
  func execute(_ sql: String) throws {
    guard let handle else {
      throw StorageError.sqlite(code: SQLITE_MISUSE, message: "connection is closed")
    }
    var error: UnsafeMutablePointer<CChar>?
    let code = sqlite3_exec(handle, sql, nil, nil, &error)
    guard code == SQLITE_OK else {
      let message = error.map { String(cString: $0) } ?? errorMessage
      sqlite3_free(error)
      throw StorageError.sqlite(code: code, message: message)
    }
  }

  /// Run one statement that changes no rows we care to read back.
  func run(_ sql: String, _ bindings: [SQLiteValue] = []) throws {
    let statement = try prepare(sql)
    defer { sqlite3_finalize(statement) }
    try bind(bindings, to: statement)
    let code = sqlite3_step(statement)
    guard code == SQLITE_DONE || code == SQLITE_ROW else {
      throw lastError(code)
    }
  }

  /// Run one statement and map every row it returns.
  func query<T>(
    _ sql: String,
    _ bindings: [SQLiteValue] = [],
    row transform: (Row) throws -> T
  ) throws -> [T] {
    let statement = try prepare(sql)
    defer { sqlite3_finalize(statement) }
    try bind(bindings, to: statement)
    var rows: [T] = []
    while true {
      let code = sqlite3_step(statement)
      if code == SQLITE_DONE {
        return rows
      }
      guard code == SQLITE_ROW else {
        throw lastError(code)
      }
      try rows.append(transform(Row(statement: statement)))
    }
  }

  /// Rowid assigned by the most recent successful `INSERT`.
  var lastInsertRowID: Int64 {
    guard let handle else { return 0 }
    return sqlite3_last_insert_rowid(handle)
  }

  private func prepare(_ sql: String) throws -> OpaquePointer {
    guard let handle else {
      throw StorageError.sqlite(code: SQLITE_MISUSE, message: "connection is closed")
    }
    var statement: OpaquePointer?
    let code = sqlite3_prepare_v2(handle, sql, -1, &statement, nil)
    guard code == SQLITE_OK, let statement else {
      throw lastError(code)
    }
    return statement
  }

  private func bind(_ bindings: [SQLiteValue], to statement: OpaquePointer) throws {
    for (offset, value) in bindings.enumerated() {
      let index = Int32(offset + 1)
      let code: Int32 =
        switch value {
        case .integer(let number):
          sqlite3_bind_int64(statement, index, number)
        case .real(let number):
          sqlite3_bind_double(statement, index, number)
        case .text(let text):
          sqlite3_bind_text(statement, index, text, -1, sqliteTransient)
        case .null:
          sqlite3_bind_null(statement, index)
        }
      guard code == SQLITE_OK else {
        throw lastError(code)
      }
    }
  }
}

/// `SQLITE_TRANSIENT` tells SQLite to copy the bound bytes; the type is the
/// C macro `((sqlite3_destructor_type)-1)` which Swift cannot import.
private let sqliteTransient = unsafeBitCast(
  -1,
  to: sqlite3_destructor_type.self
)
