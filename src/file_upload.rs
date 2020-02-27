use actix_web::{
    dev,
    http::{header, StatusCode},
    multipart, FutureResponse, HttpMessage, HttpRequest, HttpResponse,
};
use futures::{future, future::FutureResult, Future, Stream};
use std::{
    fs,
    io::Write,
    path::{Component, PathBuf},
};

use crate::errors::{self, ContextualError};
use crate::listing::{self, SortingMethod, SortingOrder};
use crate::renderer;
use crate::themes::ColorScheme;

/// Create future to save file.
fn save_file(
    field: multipart::Field<dev::Payload>,
    file_path: PathBuf,
    overwrite_files: bool,
) -> Box<dyn Future<Item = i64, Error = ContextualError>> {
    if !overwrite_files && file_path.exists() {
        return Box::new(future::err(ContextualError::CustomError(
            "File already exists, and the overwrite_files option has not been set".to_string(),
        )));
    }

    let mut file = match std::fs::File::create(&file_path) {
        Ok(file) => file,
        Err(e) => {
            return Box::new(future::err(ContextualError::IOError(
                format!("Failed to create {}", file_path.display()),
                e,
            )));
        }
    };
    Box::new(
        field
            .map_err(ContextualError::MultipartError)
            .fold(0i64, move |acc, bytes| {
                let rt = file
                    .write_all(bytes.as_ref())
                    .map(|_| acc + bytes.len() as i64)
                    .map_err(|e| {
                        ContextualError::IOError("Failed to write to file".to_string(), e)
                    });
                future::result(rt)
            }),
    )
}

/// Create new future to handle file as multipart data.
fn handle_multipart(
    item: multipart::MultipartItem<dev::Payload>,
    mut file_path: PathBuf,
    overwrite_files: bool,
) -> Box<dyn Stream<Item = i64, Error = ContextualError>> {
    match item {
        multipart::MultipartItem::Field(field) => {
            let filename = field
                .headers()
                .get(header::CONTENT_DISPOSITION)
                .ok_or(ContextualError::ParseError)
                .and_then(|cd| {
                    header::ContentDisposition::from_raw(cd)
                        .map_err(|_| ContextualError::ParseError)
                })
                .and_then(|content_disposition| {
                    content_disposition
                        .get_filename()
                        .ok_or(ContextualError::ParseError)
                        .map(String::from)
                });
            let err = |e: ContextualError| Box::new(future::err(e).into_stream());
            match filename {
                Ok(f) => {
                    match fs::metadata(&file_path) {
                        Ok(metadata) => {
                            if !metadata.is_dir() {
                                return err(ContextualError::InvalidPathError(format!(
                                    "cannot upload file to {}, since it's not a directory",
                                    &file_path.display()
                                )));
                            } else if metadata.permissions().readonly() {
                                return err(ContextualError::InsufficientPermissionsError(
                                    file_path.display().to_string(),
                                ));
                            }
                        }
                        Err(_) => {
                            return err(ContextualError::InsufficientPermissionsError(
                                file_path.display().to_string(),
                            ));
                        }
                    }
                    file_path = file_path.join(f);
                    Box::new(save_file(field, file_path, overwrite_files).into_stream())
                }
                Err(e) => err(e(
                    "HTTP header".to_string(),
                    "Failed to retrieve the name of the file to upload".to_string(),
                )),
            }
        }
        multipart::MultipartItem::Nested(mp) => Box::new(
            mp.map_err(ContextualError::MultipartError)
                .map(move |item| handle_multipart(item, file_path.clone(), overwrite_files))
                .flatten(),
        ),
    }
}

/// Handle incoming request to upload file.
/// Target file path is expected as path parameter in URI and is interpreted as relative from
/// server root directory. Any path which will go outside of this directory is considered
/// invalid.
/// This method returns future.
pub fn upload_file(
    req: &HttpRequest<crate::MiniserveConfig>,
    default_color_scheme: ColorScheme,
    uses_random_route: bool
) -> FutureResponse<HttpResponse> {
    let return_path = if let Some(header) = req.headers().get(header::REFERER) {
        header.to_str().unwrap_or("/").to_owned()
    } else {
        "/".to_string()
    };

    let query_params = listing::extract_query_parameters(req);
    let color_scheme = query_params.theme.unwrap_or(default_color_scheme);
    let upload_path = match query_params.path.clone() {
        Some(path) => match path.strip_prefix(Component::RootDir) {
            Ok(stripped_path) => stripped_path.to_owned(),
            Err(_) => path.clone(),
        },
        None => {
            let err = ContextualError::InvalidHTTPRequestError(
                "Missing query parameter 'path'".to_string(),
            );
            return Box::new(create_error_response(
                &err.to_string(),
                StatusCode::BAD_REQUEST,
                &return_path,
                query_params.sort,
                query_params.order,
                color_scheme,
                default_color_scheme,
                uses_random_route
            ));
        }
    };

    let app_root_dir = match req.state().path.canonicalize() {
        Ok(dir) => dir,
        Err(e) => {
            let err = ContextualError::IOError(
                "Failed to resolve path served by miniserve".to_string(),
                e,
            );
            return Box::new(create_error_response(
                &err.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
                &return_path,
                query_params.sort,
                query_params.order,
                color_scheme,
                default_color_scheme,
                uses_random_route
            ));
        }
    };

    // If the target path is under the app root directory, save the file.
    let target_dir = match &app_root_dir.clone().join(upload_path).canonicalize() {
        Ok(path) if path.starts_with(&app_root_dir) => path.clone(),
        _ => {
            let err = ContextualError::InvalidHTTPRequestError(
                "Invalid value for 'path' parameter".to_string(),
            );
            return Box::new(create_error_response(
                &err.to_string(),
                StatusCode::BAD_REQUEST,
                &return_path,
                query_params.sort,
                query_params.order,
                color_scheme,
                default_color_scheme,
                uses_random_route
            ));
        }
    };
    let overwrite_files = req.state().overwrite_files;
    Box::new(
        req.multipart()
            .map_err(ContextualError::MultipartError)
            .map(move |item| handle_multipart(item, target_dir.clone(), overwrite_files))
            .flatten()
            .collect()
            .then(move |e| match e {
                Ok(_) => future::ok(
                    HttpResponse::SeeOther()
                        .header(header::LOCATION, return_path)
                        .finish(),
                ),
                Err(e) => create_error_response(
                    &e.to_string(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &return_path,
                    query_params.sort,
                    query_params.order,
                    color_scheme,
                    default_color_scheme,
                    uses_random_route
                ),
            }),
    )
}

/// Convenience method for creating response errors, if file upload fails.
#[allow(clippy::too_many_arguments)]
fn create_error_response(
    description: &str,
    error_code: StatusCode,
    return_path: &str,
    sorting_method: Option<SortingMethod>,
    sorting_order: Option<SortingOrder>,
    color_scheme: ColorScheme,
    default_color_scheme: ColorScheme,
    uses_random_route: bool
) -> FutureResult<HttpResponse, actix_web::error::Error> {
    errors::log_error_chain(description.to_string());
    future::ok(
        HttpResponse::BadRequest()
            .content_type("text/html; charset=utf-8")
            .body(
                renderer::render_error(
                    description,
                    error_code,
                    return_path,
                    sorting_method,
                    sorting_order,
                    color_scheme,
                    default_color_scheme,
                    true,
                    !uses_random_route,
                )
                .into_string(),
            ),
    )
}
